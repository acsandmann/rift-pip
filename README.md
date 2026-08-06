# rift-pip

a minimal [rift](https://github.com/atticus/rift) plugin that allows any window to be mirrored into a PiP window

<img src="assets/pip.png" alt="rift-pip" />

### Installation

```sh
git clone https://github.com/atticus/rift-pip
cd rift-pip
cargo install --path .
```

### Usage

with the desired window focused, click the hotkeys to launch the PiP

```toml
# rift config
"Alt + Ctrl + P" = { exec = ["sh", "-c", "rift-pip"] }
```

```sh
rift-pip [--top_left|--top_right|--bottom_left|--bottom_right]
```
