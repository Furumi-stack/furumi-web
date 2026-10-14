use std::path::Path;

use super::dto::PathHints;

/// Parse metadata hints from the file path relative to the inbox directory.
///
/// Recognized patterns:
///   Artist/Album/01 - Title.ext
///   Artist/Album (Year)/01 - Title.ext
///   Artist/(Year) Album/01 - Title.ext
///   Artist/Album [Year]/01 - Title.ext
///   01 - Title.ext  (flat, no artist/album)
pub fn parse(relative_path: &Path) -> PathHints {
    let components: Vec<&str> = relative_path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    let mut hints = PathHints::default();

    let filename = components.last().copied().unwrap_or("");
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // Parse track number and title from filename
    parse_filename(stem, &mut hints);

    match components.len() {
        // Artist/.../Album/file.ext: use the nearest release folder rather
        // than an organizational directory such as "Альбомы и синглы".
        3.. => {
            let mut album_index = components.len() - 2;
            if album_index > 0 && is_disc_folder(components[album_index]) {
                album_index -= 1;
            }
            hints.artist = components[..album_index]
                .iter()
                .rev()
                .find(|name| !is_organizational_folder(name))
                .map(|name| (*name).to_owned());
            let album_raw = components[album_index];
            let (album, year) = parse_album_with_year(album_raw);
            if !is_organizational_folder(&album) {
                hints.album = Some(album);
            }
            if year.is_some() {
                hints.year = year;
            }
        }
        // Album/file.ext (or Artist/file.ext — ambiguous, treat as album)
        2 => {
            let dir = components[0];
            let (name, year) = parse_album_with_year(dir);
            if !is_organizational_folder(&name) {
                hints.album = Some(name);
            }
            if year.is_some() {
                hints.year = year;
            }
        }
        // Just file.ext
        _ => {}
    }

    hints
}

fn is_organizational_folder(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "music"
            | "downloads"
            | "albums"
            | "singles"
            | "albums and singles"
            | "mixed"
            | "mix"
            | "музыка"
            | "загрузки"
            | "альбомы"
            | "синглы"
            | "альбомы и синглы"
            | "разное"
    )
}

fn is_disc_folder(name: &str) -> bool {
    let name = name.trim().to_lowercase();
    ["cd", "disc", "disk", "диск"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|number| number.trim().parse::<u32>().ok())
            .is_some_and(|number| number > 0)
    })
}

/// Try to extract track number and title from a filename stem.
///
/// Patterns: "01 - Title", "01. Title", "1 Title", "Title"
fn parse_filename(stem: &str, hints: &mut PathHints) {
    let trimmed = stem.trim();

    // Try "NN - Title" or "NN. Title"
    if let Some(rest) = try_strip_track_prefix(trimmed) {
        let (num_str, title) = rest;
        if let Ok(num) = num_str.parse::<i32>() {
            hints.track_number = Some(num);
            if !title.is_empty() {
                hints.title = Some(title.to_owned());
            }
            return;
        }
    }

    // No track number found, use full stem as title
    if !trimmed.is_empty() {
        hints.title = Some(trimmed.to_owned());
    }
}

/// Try to parse "NN - Rest" or "NN. Rest" from a string.
/// Returns (number_str, rest) if successful.
fn try_strip_track_prefix(s: &str) -> Option<(&str, &str)> {
    let digit_end = s.find(|c: char| !c.is_ascii_digit())?;
    if digit_end == 0 {
        return None;
    }
    let num_str = &s[..digit_end];
    let rest = s[digit_end..].trim_start();

    let title = if let Some(stripped) = rest.strip_prefix("- ") {
        stripped.trim()
    } else if let Some(stripped) = rest.strip_prefix(". ") {
        stripped.trim()
    } else if let Some(stripped) = rest.strip_prefix('.') {
        stripped.trim()
    } else {
        rest
    };

    Some((num_str, title))
}

/// Extract album name and optional year from directory name.
///
/// Patterns: "Album (2001)", "(2001) Album", "Album [2001]", "Album"
fn parse_album_with_year(dir: &str) -> (String, Option<i32>) {
    // Common discography folders: "2002 - Album".
    if let Some((prefix, album)) = dir.split_once(" - ")
        && let Ok(year) = prefix.trim().parse::<i32>()
        && (1900..=2100).contains(&year)
        && !album.trim().is_empty()
    {
        return (album.trim().to_owned(), Some(year));
    }
    // Try "Album (YYYY)" or "Album [YYYY]"
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(start) = dir.rfind(open) {
            if let Some(end) = dir[start..].find(close) {
                let inside = &dir[start + 1..start + end];
                if let Ok(year) = inside.trim().parse::<i32>() {
                    if (1900..=2100).contains(&year) {
                        let album =
                            format!("{}{}", &dir[..start].trim(), &dir[start + end + 1..].trim());
                        let album = album.trim().to_owned();
                        return (album, Some(year));
                    }
                }
            }
        }
    }

    // Try "(YYYY) Album"
    if dir.starts_with('(') {
        if let Some(end) = dir.find(')') {
            let inside = &dir[1..end];
            if let Ok(year) = inside.trim().parse::<i32>() {
                if (1900..=2100).contains(&year) {
                    let album = dir[end + 1..].trim().to_owned();
                    return (album, Some(year));
                }
            }
        }
    }

    (dir.to_owned(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_artist_album_track() {
        let p = PathBuf::from("Pink Floyd/Wish You Were Here (1975)/03 - Have a Cigar.flac");
        let h = parse(&p);
        assert_eq!(h.artist.as_deref(), Some("Pink Floyd"));
        assert_eq!(h.album.as_deref(), Some("Wish You Were Here"));
        assert_eq!(h.year, Some(1975));
        assert_eq!(h.track_number, Some(3));
        assert_eq!(h.title.as_deref(), Some("Have a Cigar"));
    }

    #[test]
    fn test_year_prefix() {
        let p = PathBuf::from("Artist/(2020) Album Name/01. Song.flac");
        let h = parse(&p);
        assert_eq!(h.artist.as_deref(), Some("Artist"));
        assert_eq!(h.album.as_deref(), Some("Album Name"));
        assert_eq!(h.year, Some(2020));
        assert_eq!(h.track_number, Some(1));
        assert_eq!(h.title.as_deref(), Some("Song"));
    }

    #[test]
    fn test_flat_file() {
        let p = PathBuf::from("05 - Something.mp3");
        let h = parse(&p);
        assert_eq!(h.artist, None);
        assert_eq!(h.album, None);
        assert_eq!(h.track_number, Some(5));
        assert_eq!(h.title.as_deref(), Some("Something"));
    }

    #[test]
    fn test_no_track_number() {
        let p = PathBuf::from("Artist/Album/Song Name.flac");
        let h = parse(&p);
        assert_eq!(h.track_number, None);
        assert_eq!(h.title.as_deref(), Some("Song Name"));
    }

    #[test]
    fn test_square_bracket_year() {
        let p = PathBuf::from("Band/Album [1999]/track.flac");
        let h = parse(&p);
        assert_eq!(h.album.as_deref(), Some("Album"));
        assert_eq!(h.year, Some(1999));
    }

    #[test]
    fn nested_discography_uses_the_release_folder() {
        let h = parse(Path::new(
            "Diskoteka_Avariya-Diskografia/Альбомы и синглы/2002 - Х.Х.Х.И.Р.Н.Р/08 - Где же ваши руки.mp3",
        ));
        assert_eq!(h.artist.as_deref(), Some("Diskoteka_Avariya-Diskografia"));
        assert_eq!(h.album.as_deref(), Some("Х.Х.Х.И.Р.Н.Р"));
        assert_eq!(h.year, Some(2002));
        assert_eq!(h.track_number, Some(8));
    }

    #[test]
    fn mixed_folder_does_not_supply_an_album() {
        let h = parse(Path::new("Mix/Track.mp3"));
        assert_eq!(h.artist, None);
        assert_eq!(h.album, None);
        assert_eq!(h.title.as_deref(), Some("Track"));
    }

    #[test]
    fn collection_root_does_not_replace_the_artist() {
        let h = parse(Path::new("Music/Artist/2020 - Album/01 - Song.flac"));
        assert_eq!(h.artist.as_deref(), Some("Artist"));
        assert_eq!(h.album.as_deref(), Some("Album"));
        assert_eq!(h.year, Some(2020));
    }

    #[test]
    fn disc_subfolder_keeps_the_album_and_artist() {
        let h = parse(Path::new("Artist/2002 - Album/CD 1/01 - Song.flac"));
        assert_eq!(h.artist.as_deref(), Some("Artist"));
        assert_eq!(h.album.as_deref(), Some("Album"));
        assert_eq!(h.year, Some(2002));
    }
}
