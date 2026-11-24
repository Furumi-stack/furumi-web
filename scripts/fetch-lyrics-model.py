"""Pre-populate Furumi's lyrics model directory for installations without internet access."""
import hashlib
import pathlib
import sys
import time
import urllib.request

REVISION = "614241f622f53c4eeff9890bdc4f31cfecc418b3"
BASE = f"https://huggingface.co/intfloat/multilingual-e5-small/resolve/{REVISION}/onnx/"
FILES = {
    "model.onnx": ("sha256", "ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665"),
    "tokenizer.json": ("sha256", "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39"),
    "config.json": ("git", "698e598c24523f3857295b11abec0703d19ea180"),
    "special_tokens_map.json": ("git", "e0b1d18ecd0ae4ff1d47bd297d910c0cf83e504b"),
    "tokenizer_config.json": ("git", "059214673d9d6d2ee319411e2ffec8c024b816d5"),
}


def digest(path, kind):
    h = hashlib.sha256() if kind == "sha256" else hashlib.sha1()
    if kind == "git":
        h.update(f"blob {path.stat().st_size}\0".encode())
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def main():
    output = pathlib.Path(sys.argv[1])
    output.mkdir(parents=True, exist_ok=True)
    for name, (kind, expected) in FILES.items():
        target = output / name
        if target.exists() and digest(target, kind) == expected:
            continue
        temp = output / (name + ".part")
        for attempt in range(4):
            try:
                with urllib.request.urlopen(BASE + name, timeout=120) as response, temp.open("wb") as dest:
                    while block := response.read(1024 * 1024):
                        dest.write(block)
                if digest(temp, kind) != expected:
                    raise ValueError(f"Checksum mismatch: {name}")
                temp.replace(target)
                print(f"Verified {name}", flush=True)
                break
            except Exception:
                temp.unlink(missing_ok=True)
                if attempt == 3:
                    raise
                time.sleep(2 ** attempt)
    (output / "NOTICE").write_text(
        "intfloat/multilingual-e5-small\nMIT License\n"
        f"https://huggingface.co/intfloat/multilingual-e5-small/tree/{REVISION}\n"
        "Copyright (c) Microsoft Corporation.\n\n"
        "Permission is hereby granted, free of charge, to any person obtaining a copy "
        "of this software and associated documentation files (the 'Software'), to deal "
        "in the Software without restriction, including without limitation the rights "
        "to use, copy, modify, merge, publish, distribute, sublicense, and/or sell "
        "copies of the Software, and to permit persons to whom the Software is "
        "furnished to do so, subject to the following conditions:\n\n"
        "The above copyright notice and this permission notice shall be included in "
        "all copies or substantial portions of the Software.\n\n"
        "THE SOFTWARE IS PROVIDED 'AS IS', WITHOUT WARRANTY OF ANY KIND, EXPRESS OR "
        "IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, "
        "FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE "
        "AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER "
        "LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING "
        "FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS "
        "IN THE SOFTWARE.\n", encoding="utf-8")


if __name__ == "__main__":
    main()
