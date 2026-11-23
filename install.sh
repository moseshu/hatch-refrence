#!/usr/bin/env bash
# hatch 一键安装：构建 release 二进制并安装到 ~/.local/bin（可用 HATCH_INSTALL_DIR 覆盖）。
# 用法：./install.sh [--check-only]
set -euo pipefail

INSTALL_DIR="${HATCH_INSTALL_DIR:-$HOME/.local/bin}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

# 1. 检查 Rust 工具链（需要 >= 1.80）
if ! command -v cargo >/dev/null 2>&1; then
  die "未找到 cargo。请先安装 Rust：https://rustup.rs（curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh）"
fi
RUST_VER="$(rustc --version | awk '{print $2}')"
info "Rust $RUST_VER"

# 2. 检查 C 链接器（SQLite 是 bundled 编译，需要 cc）
command -v cc >/dev/null 2>&1 || die "未找到 cc（C 编译器）。Debian/Ubuntu: sudo apt install build-essential；macOS: xcode-select --install"

[ "${1:-}" = "--check-only" ] && { info "环境检查通过"; exit 0; }

# 3. 构建（release）
info "构建 hatch（release，首次约需数分钟）..."
cd "$REPO_DIR"
cargo build --release -p hatch-cli

# 4. 安装
mkdir -p "$INSTALL_DIR"
install -m 0755 "target/release/hatch" "$INSTALL_DIR/hatch"
info "已安装到 $INSTALL_DIR/hatch"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "$INSTALL_DIR 不在 PATH 里，请把它加进 shell 配置：export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

# 5. 下一步提示
cat <<'EOF'

下一步：
  1. hatch init                      # 初始化 home 目录与配置（默认 ~/.hatch）
  2. 编辑配置，填模型与密钥环境变量名（见 config/hatch.example.toml）
  3. export HATCH_LLM_API_KEY=...    # 密钥只走环境变量，不进配置文件
  4. hatch doctor                    # 自检
  5. hatch ask "你好"                # 命令行试一轮
  6. hatch daemon run --api          # 常驻运行 + 产品 API（默认 127.0.0.1:8787）
EOF
