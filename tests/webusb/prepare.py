"""Copy source into disposable scratch and add visibility, never alter production."""
import pathlib
import shutil
import sys

repo = pathlib.Path(__file__).resolve().parents[2]
scratch = pathlib.Path(sys.argv[1]).resolve()
scratch.mkdir(parents=True, exist_ok=False)
for name in ("src", "assets", ".cargo", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "index.html", "Trunk.toml"):
    source = repo / name
    if source.is_dir():
        shutil.copytree(source, scratch / name)
    elif source.exists():
        shutil.copy2(source, scratch / name)
with (scratch / "src/usb_wasm.rs").open("a") as f:
    f.write("\n" + (repo / "tests/webusb/adapter.rs").read_text())
