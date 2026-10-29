# omniroute-rust 部署指南（Linux / WSL2 从零部署）

单 crate Rust 网关：OpenAI / Claude / Gemini 兼容端点，默认监听 `127.0.0.1:20128`，
数据目录默认 `~/.omniroute-rust`。本指南面向**另一台本地电脑（Ubuntu 22.04+ / WSL2，
或 macOS）从零部署**，重点覆盖：

- 构建前置（`boring-sys` 用 CMake 编译自带 BoringSSL，需要 cmake ≥ 3.22 与 C++ 工具链）
- 全新机器部署（用户级 systemd 服务 / 前台调试 / system 服务差异）
- 免费 lane（OpenCode Zen 匿名档）最小可用配置
- 出口代理池（免费额度按**出口 IP** 记账，按 IP 扩容）
- 预算 / 会话 / 回退旋钮，验证清单，故障排查，升级与回滚

> 安全约定：文中所有主机、IP、端口、口令、API key、会话 id 均为**占位符**
> （`<proxy-host>`、`<user>`、`<pass>`、`<api-key>` 等）。`$DATA_DIR` 内的
> `provider-credentials.json`、`dashboard-auth.json`、`opencode-free-*.json`，以及含代理
> 口令的 `.env` / `omniroute.toml` 都不得提交仓库；含口令的文件保持 `0600`。

推荐目录布局（与现有主机一致，`$HOME` 因用户而异）：

| 项 | 路径 |
|---|---|
| 二进制 | `~/.local/bin/omniroute`（release 构建） |
| 数据目录 | `~/.omniroute-rust/`（provider-credentials.json、.env、omniroute.toml、免费 lane 状态文件） |
| 服务单元 | `~/.config/systemd/user/omniroute-rust.service`（`deploy/omniroute-rust.service` 为仓库内副本） |

## 0. 十行速查

```bash
# 1) 构建依赖：cmake 必须 >= 3.22（Ubuntu 22.04 自带 3.22.1）
sudo apt update && sudo apt install -y build-essential cmake git curl jq && cmake --version

# 2) Rust stable（rustup）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh && source "$HOME/.cargo/env"

# 3) 拉源码
git clone https://github.com/liudasheng/omniroute-rust.git && cd omniroute-rust

# 4) 构建（首次 5–20 分钟，BoringSSL 占大头；产物 ≈ 84 MB）
cargo build --release

# 5) 准备目录（数据目录仅本人可读写）
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -d -m 700 "$HOME/.omniroute-rust"

# 6) 安装二进制与用户服务单元
install -m 755 target/release/omniroute "$HOME/.local/bin/omniroute"
cp deploy/omniroute-rust.service "$HOME/.config/systemd/user/omniroute-rust.service"

# 7) 启动并配置开机自启（WSL 需先启用 systemd，见 §2.1）
systemctl --user daemon-reload && systemctl --user enable --now omniroute-rust
loginctl enable-linger "$USER"

# 8) 健康检查（首次登录密码默认 CHANGEME，见 §3）
curl -fsS http://127.0.0.1:20128/healthz && curl -fsS http://127.0.0.1:20128/v1/models >/dev/null

# 9) 免费模型自检（带 "free": true；设了 OMNIROUTE_API_KEY 时加 -H 'Authorization: Bearer <api-key>'）
curl -sS http://127.0.0.1:20128/v1/models | jq -r '.data[] | select(.free) | .id' | head

# 10) 免费 lane 出口/预算按需配置（见 §5/§6；drop-in 见 §9.2）；含口令的文件 0600
install -m 600 deploy/opencode-free.env.example "$HOME/.omniroute-rust/opencode-free.env"
```

## 1. 构建前置

### 1.1 依赖清单

| 依赖 | 版本 / 说明 |
|---|---|
| Rust | stable（rustup 安装；edition 2021） |
| git | clone 源码 |
| **cmake** | **≥ 3.22**。`Cargo.toml` 的 `boring = "5.3"` 会拉 `boring-sys`，后者用 CMake 编译自带的 BoringSSL；其 `CMakeLists.txt` 要求 `cmake_minimum_required(VERSION 3.22)` |
| C++ 工具链 | `g++` / `clang++`（编译 BoringSSL；Ubuntu 用 `build-essential`） |
| make / ninja | CMake 默认生成器需要（`build-essential` 已含 make） |
| jq（可选） | 本指南的部分自检命令使用 |
| 系统 OpenSSL | **不需要**：BoringSSL 由 crate 自带并静态链接；运行时只依赖 glibc（含 libpthread/libm/libdl）、libstdc++、libgcc_s |

### 1.2 Ubuntu 22.04 / WSL2（Ubuntu 22.04+）

```bash
sudo apt update
sudo apt install -y build-essential cmake git curl jq
cmake --version        # 必须 >= 3.22；Ubuntu 22.04 为 3.22.1

# Rust stable
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

### 1.3 cmake 版本不足时（如 Ubuntu 20.04 自带 3.16）

两种方式任选其一，装完 `cmake --version` 必须 ≥ 3.22：

```bash
# 方式一：PyPI 的 cmake 二进制（安装到 ~/.local/bin）
python3 -m pip install --user 'cmake>=3.22'
export PATH="$HOME/.local/bin:$PATH"     # 写入 ~/.bashrc 可持久化
cmake --version

# 方式二：Kitware 官方二进制（版本号按需替换，需存在于 releases）
version=3.31.6
curl -fsSLO "https://github.com/Kitware/CMake/releases/download/v${version}/cmake-${version}-linux-x86_64.tar.gz"
sudo tar -xzf "cmake-${version}-linux-x86_64.tar.gz" -C /opt
sudo ln -sf "/opt/cmake-${version}-linux-x86_64/bin/cmake" /usr/local/bin/cmake
cmake --version
```

> 注意：`pip install` 的 `cmake` 需要能 import；若 `cmake --version` 报
> `ModuleNotFoundError: No module named 'cmake'`，说明解释器/环境不匹配，
> 换用 Kitware 二进制更稳。

### 1.4 macOS

```bash
xcode-select --install            # Command Line Tools（含 clang 与 make）
brew install cmake git
# Rust stable
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 1.5 构建与产物

```bash
git clone https://github.com/liudasheng/omniroute-rust.git
cd omniroute-rust
cargo build --release

ls -lh target/release/omniroute       # ≈ 84 MB（release，未 strip）
./target/release/omniroute --version
```

- 预计耗时：**首次全量约 5–20 分钟**（取决于 CPU 核数与网络；BoringSSL 编译占大头），
  之后增量重建通常 1 分钟以内。
- 产物路径：`target/release/omniroute`；运行期动态依赖只有 glibc（含 libpthread/libm/libdl）、
  libstdc++、libgcc_s（`ldd target/release/omniroute` 可核对）。
- 构建需要临时磁盘（仅 release 构建的 `target/` 约 2–3 GB；再加 debug 构建会更大）与足够内存；
  WSL2 默认内存通常够用。

### 1.6 只在构建机出二进制（部署机免工具链）

构建机与部署机可以分离：部署机只需要**同架构、且 glibc 不旧于构建机**的 Linux。

```bash
# 构建机
cargo build --release
sha256sum target/release/omniroute

# 部署机（占位符）
scp target/release/omniroute "<user>@<deploy-host>:/tmp/omniroute"
install -m 755 /tmp/omniroute "$HOME/.local/bin/omniroute"
```

- **glibc 兼容方向**：构建机的 glibc **不得高于**运行机，否则运行时报
  `version 'GLIBC_2.xx' not found`。目标若是 Ubuntu 22.04，就在 22.04（或更老）上构建；
  `ldd --version | head -1` 可查本机 glibc。
- macOS 产物（Mach-O）**不能**拷到 Linux 运行；反向也不行。
- 架构必须一致：`file target/release/omniroute` 查看是 `x86_64` 还是 `aarch64`。
- 构建机与部署机同为 Ubuntu 22.04/24.04 最省心。

## 2. 全新机器部署

### 2.1 systemd 用户服务（推荐，与仓库模板一致）

```bash
git clone https://github.com/liudasheng/omniroute-rust.git
cd omniroute-rust
cargo build --release

install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -d -m 700 "$HOME/.omniroute-rust"
install -m 755 target/release/omniroute "$HOME/.local/bin/omniroute"
cp deploy/omniroute-rust.service "$HOME/.config/systemd/user/omniroute-rust.service"

systemctl --user daemon-reload
systemctl --user enable --now omniroute-rust
systemctl --user status omniroute-rust --no-pager
curl -fsS http://127.0.0.1:20128/healthz      # → ok
```

- `deploy/omniroute-rust.service` 里 `ExecStart=%h/.local/bin/omniroute serve`、
  `Environment=OMNIROUTE_DATA_DIR=%h/.omniroute-rust`，`%h` 指当前用户 home。
- 若希望**未登录时也常驻**（WSL / 无桌面机器），启用 linger：
  `loginctl enable-linger "$USER"`（被 polkit 拒绝时用 `sudo loginctl enable-linger "$USER"`）。
- **WSL2 前提**：需要 systemd。若 `systemctl --user status` 报
  "System has not been booted with systemd"，先在 WSL 内写 `/etc/wsl.conf`：

```bash
sudo tee /etc/wsl.conf >/dev/null <<'EOF'
[boot]
systemd=true
EOF
```

  然后在 Windows PowerShell 执行 `wsl --shutdown`，再重新打开 WSL 即可。
- 端口/监听地址默认 `127.0.0.1:20128`；改端口用 `--port` / `PORT`，局域网访问需
  `HOST=0.0.0.0`（注意暴露范围与鉴权，见 §14）。

### 2.2 前台运行（调试用）

不装服务、直接跑二进制（数据目录仍用 `~/.omniroute-rust`）：

```bash
OMNIROUTE_DATA_DIR="$HOME/.omniroute-rust" \
  "$HOME/.local/bin/omniroute" serve --port 20128

# 或从仓库直接跑构建产物
./target/release/omniroute serve --port 20129      # 默认监听 127.0.0.1
```

`omniroute serve --help` 显示 `--port`、`--host`、`--api-key` 等参数；
前台运行时 `Ctrl-C` 退出，`omniroute stop`（读取 `$DATA_DIR/omniroute.pid`）可停掉
同数据目录启动的实例。**调试实例请另开端口与数据目录**，不要指向线上数据目录。

### 2.3 system 服务（与仓库用户模板的差异）

仓库模板是**用户级**单元，`%h` 会展开为当前用户 home；改成 system 服务时必须替换：

```ini
# /etc/systemd/system/omniroute-rust.service
[Unit]
Description=OmniRoute-Rust AI Gateway
Wants=network-online.target
After=network-online.target

[Service]
User=<service-user>
Group=<service-user>
ExecStart=/usr/local/bin/omniroute serve
Environment=OMNIROUTE_DATA_DIR=/var/lib/omniroute
# 可选：匿名免费 lane 出口池（示例，按需替换或删除整行）
Environment=OMNIROUTE_OPENCODE_FREE_PROXY_POOL=http://<user>:<pass>@<proxy-host>:3128
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

```bash
# 占位符 <service-user> 请换成实际用户；数据目录属主与权限要对
sudo install -d -m 700 -o "<service-user>" -g "<service-user>" /var/lib/omniroute
sudo install -m 755 target/release/omniroute /usr/local/bin/omniroute
sudo systemctl daemon-reload
sudo systemctl enable --now omniroute-rust
sudo systemctl status omniroute-rust --no-pager
# 日志用 system 级 journal：
journalctl -u omniroute-rust -f
```

差异要点：用户服务用 `systemctl --user` + `journalctl --user`，system 服务用
`sudo systemctl` + `journalctl -u`；`%h` 在 system 单元里不可依赖，一律写绝对路径；
数据目录与 env 文件必须让服务用户可读（`0600` 属主为该用户）。

### 2.4 目录与权限

```bash
chmod 700 "$HOME/.omniroute-rust"
chmod 600 "$HOME/.omniroute-rust/provider-credentials.json" 2>/dev/null || true
chmod 600 "$HOME/.omniroute-rust/.env" 2>/dev/null || true
# omniroute.toml 只有在含代理口令/密钥时才需要 0600
```

- `provider-credentials.json` 由网关以 `0600` 原子写入；手工粘贴 key 后请自行确认权限。
- 含代理口令的 `.env` / `omniroute.toml` / `opencode-free.env` **必须 `0600`**。
- 免费 lane 的 `opencode-free-budget.json` / `opencode-free-sessions.json` 由网关自动以
  `0600` 原子写入；删除即重置（见 §5）。

## 3. 首次登录与管理员密码

首次部署后管理员密码默认为 **CHANGEME**（与原版一致）。打开 `http://127.0.0.1:20128/dashboard`
会显示密码输入框，也可以在仪表盘顶部横幅里立即改密。

```bash
# 服务日志里也会提醒
journalctl --user -u omniroute-rust -n 10 | grep CHANGEME
```

改密方式（三选一）：

1. 仪表盘顶部横幅 → 输入新密码 → change now
2. `curl -X POST http://127.0.0.1:20128/v1/auth/change-password -H ... -d '{"current_password":"CHANGEME","new_password":"..."}'`
3. 下次启动带环境变量：`systemctl --user edit omniroute-rust` 加
   `Environment=OMNIROUTE_ADMIN_PASSWORD=你的密码` 并 restart

密码哈希（加盐 SHA-256，无明文，权限 600）位于 `~/.omniroute-rust/dashboard-auth.json`；
部署级覆盖是 systemd 单元里的 `Environment=OMNIROUTE_ADMIN_PASSWORD=...`。
**忘了密码 / 改完登不上时重置**（服务运行时也可，立即生效）：

```bash
omniroute reset-password --password '你的新密码'      # 至少 8 位
printf '新密码\n新密码\n' | omniroute reset-password   # 管道模式（第一行密码，第二行确认）
omniroute reset-password --password-stdin <<< '新密码'  # 整段 stdin 作为密码
```

重置写的是 `$DATA_DIR/dashboard-auth.json`，网关会热读取该文件，无需重启。

> 安全提醒：若仪表盘端口对公网开放（如 0.0.0.0:20128），务必先改掉默认的 `CHANGEME`。

## 4. 免费 lane 最小可用配置（OpenCode Zen 匿名档）

### 4.1 零配置即可用

`opencode` / `opencode-zen` 两个 provider 用字面量匿名凭据 `public` 即可路由，
**免注册、免充值、无需在仪表盘创建 provider connection**：

- 名册来自实时目录（`GET https://opencode.ai/zen/v1/models`）∩ models.dev 零成本判定，
  自动剔除本地黑名单与本网关实测到的封锁 id。
- `/v1/models` 对免费模型标 `"free": true` / `"freeLane": true`（两个字段同值）。
- 首次请求可能明显偏慢：名册发现、BoringSSL 首次握手、会话建立都在此时发生；
  可先用 `GET /v1/models` 或 `POST /v1/opencode-free/refresh` 预热名册。
- 新机器出口 IP 若还有额度，通常直接返回 200；若 429，按 §8 判断是哪种 429。

```bash
# 列出当前免费模型 id（设了 OMNIROUTE_API_KEY 时加 -H "Authorization: Bearer <api-key>"）
curl -sS http://127.0.0.1:20128/v1/models | jq -r '.data[] | select(.free) | .id'

# 流式请求一次（模型 id 以 /v1/models 实际返回为准；flint-40b-free 为示例）
curl -sS -N -i --max-time 120 http://127.0.0.1:20128/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"opencode/flint-40b-free","stream":true,"messages":[{"role":"user","content":"Reply with the word: pong"}]}' \
  | head -c 800
# 期望：HTTP/1.1 200，正文为 SSE（若干 data: {...} 行）
```

免费 lane 会强制流式、补齐官方 agent 形状（官方 system prompt + 官方 `read` 门禁工具），
并使用 Bun 形状的 BoringSSL ClientHello；这些都由网关自动完成，客户端无需配合。

### 4.2 可选的最小 `omniroute.toml`

匿名线本身不需要任何配置。若要把预算/会话/出口写进文件（而不是环境变量）：

```toml
# $DATA_DIR/omniroute.toml
[opencode_free]
# 本机首次部署建议把本地日上限调高，让 learned_limit 从真实 429 学习真实上限
daily_limit_per_account = 10000
daily_limit_public = 10000

# 出口代理池（可选，逗号分隔的 env 变量优先于此数组）；详见 §5
# proxy_pool = [
#   "http://<user>:<pass>@<proxy-host>:3128",
#   "socks5h://<user>:<pass>@<socks-host>:1080",
# ]
# max_proxy_exits = 4
# proxy_cooldown_secs = 30
```

> 完整示例（含 combo、providers 等）见仓库 `examples/omniroute.toml`。
> 含代理口令时 `chmod 600 "$HOME/.omniroute-rust/omniroute.toml"`。

## 5. 为什么需要出口代理（关键）

### 5.1 记账规则

- 免费额度按**出口 IP** 记账：同一 IP 耗尽后，换一个出口就是另一个额度桶。
- 多账号在**同一出口**上**不扩容**：网关若在 60 秒内看到同模型两个凭据先后收到日额度
  429，会把本轮判定为 egress-scoped 并停止逐账号试探（`bucketScope: "egress"`）。
- 上游还可能按**会话准入**拒绝：已接受（warm）的旧会话可续，新会话直接 429，
  且 `Retry-After` 可能指向很远（有误导性）。网关默认把"低用量日额度 429"视为
  疑似会话准入，只做短冷却（见 §6）。
- 结论：单机单出口的免费额度有限，要用免费 lane 稳定跑量，就要准备**多个独立出口**。

### 5.2 配置位置与优先级

三种方式任选（**环境变量优先于 TOML**；同名的带前缀/不带前缀 env 都接受，
带 `OMNIROUTE_` 前缀者优先）：

```bash
# 方式一：环境变量（逗号分隔；推荐用 systemd EnvironmentFile，见 §9.2）
OMNIROUTE_OPENCODE_FREE_PROXY_POOL='http://<user>:<pass>@<proxy-host>:3128,socks5h://<user>:<pass>@<socks-host>:1080'
```

```toml
# 方式二：$DATA_DIR/omniroute.toml
[opencode_free]
proxy_pool = [
  "http://<user>:<pass>@<proxy-host>:3128",
  "socks5h://<user>:<pass>@<socks-host>:1080",
]
max_proxy_exits = 4
proxy_cooldown_secs = 30
```

方式三：**托管账号单独出口**。给某个 `opencode` / `opencode-zen` 账号在
`$DATA_DIR/provider-connections.json` 里设置 `proxyUrl`（只作用于该账号；未设置的账号仍直连）：

```bash
# 仪表盘当前没有这个字段，只能 PATCH 或手改 JSON（两者都需管理鉴权）
curl -sS -X PATCH "http://127.0.0.1:20128/v1/provider-connections/<connection-id>" \
  -H "Authorization: Bearer <sess_或admin-key>" \
  -H 'Content-Type: application/json' \
  -d '{"proxyUrl":"socks5h://<user>:<pass>@<socks-host>:1080"}'
# 清除：-d '{"proxyUrl": null}'；手改 JSON 后需重启服务；文件必须 0600
chmod 600 "$HOME/.omniroute-rust/provider-connections.json"
```

配置落在 `.env` / `omniroute.toml` / `provider-connections.json` 时，含口令的文件一律 `0600`；
状态接口与日志中的出口地址都是脱敏的（`192.0.*.*:3128` 形式），口令不会出现。

### 5.3 方案 A：HTTP CONNECT 代理（tinyproxy / squid，Basic 认证）

网关对 `http://[user:pass@]host:port` 走 HTTP `CONNECT`，URL 的 userinfo 会转成
`Proxy-Authorization: Basic …`。最小 tinyproxy 示例（Ubuntu 22.04；`BasicAuth` 需
tinyproxy ≥ 1.10，22.04 自带 1.11，更老的版本请改用 Squid）：

```bash
sudo apt install -y tinyproxy

# /etc/tinyproxy/tinyproxy.conf 关键项（其余保持默认）
#   Port 3128
#   Listen 127.0.0.1            # 网关与代理同机时只监听回环；跨机再改成可达地址
#   Allow 127.0.0.1             # 只允许本机客户端；跨机时加 Allow <gateway-ip>
#   BasicAuth <user> <pass>     # 需要认证时；改完配置后 restart
#   ConnectPort 443             # 至少放行 443（上游是 HTTPS）

sudo systemctl enable --now tinyproxy

# 自检（应输出 200）
curl -sS -x 'http://<user>:<pass>@127.0.0.1:3128' -o /dev/null -w '%{http_code}\n' https://opencode.ai/
```

Squid 同理（HTTP CONNECT + Basic 认证）：

```bash
sudo apt install -y squid apache2-utils
sudo htpasswd -c /etc/squid/passwd "<user>"
```

然后在 `/etc/squid/squid.conf` 里确保包含（最小示例，路径按发行版核对）：

```apache
http_port 3128
auth_param basic program /usr/lib/squid/basic_ncsa_auth /etc/squid/passwd
acl authenticated proxy_auth REQUIRED
http_access allow authenticated
```

```bash
sudo systemctl restart squid
```

写入代理池即为 `http://<user>:<pass>@<proxy-host>:3128`。

### 5.4 方案 B：SOCKS5 代理

任何 SOCKS5 出口都可以；最省事的是 SSH 动态转发（出口 IP 即 SSH 服务器所在网络）：

```bash
# 本机 1080 提供 SOCKS5（-N 不执行远端命令；仅监听回环）
ssh -N -D 127.0.0.1:1080 "<user>@<ssh-host>"

# 代理池条目（ssh -D 由远端解析域名，因此用 socks5:// 而不是 socks5h://）
#   socks5://127.0.0.1:1080
# 需要认证的独立 SOCKS5 服务写成：
#   socks5h://<user>:<pass>@<socks-host>:1080
```

**scheme 语义与 curl 相反**（务必注意）：

| 网关 scheme | 域名解析位置 | 等价 curl 选项 |
|---|---|---|
| `socks5://host:port` | **代理端**解析（ATYP=域名发给代理） | `curl --socks5-hostname` |
| `socks5h://host:port` | **网关本地**解析后发 IP | `curl --socks5` |

因此：`curl --socks5-hostname …` 能通时，网关应配 `socks5://`；`curl --socks5 …` 能通时，
网关应配 `socks5h://`。`socks5` 必须显式写端口（无默认端口）。

### 5.5 方案 C：WireGuard / WARP（wgcf + wireproxy）

用 `wgcf` 生成 Cloudflare WARP 的 WireGuard 配置，再用 `wireproxy`（纯用户态、无需
root 或内核 WireGuard 模块，WSL2 可用）在本机暴露一个 SOCKS5 出口：

```bash
# 1) 安装 wgcf（从 GitHub releases 取 linux_amd64 二进制，版本按需替换）
curl -fsSL -o /tmp/wgcf \
  "https://github.com/ViRb3/wgcf/releases/download/v2.3.1/wgcf_2.3.1_linux_amd64"
install -d "$HOME/.local/bin"
install -d -m 700 "$HOME/.config/warp"
install -m 755 /tmp/wgcf "$HOME/.local/bin/wgcf"

# 2) 注册 WARP 身份并生成 WireGuard 配置
cd "$HOME/.config/warp"
wgcf register --accept-tos      # 生成 wgcf-account.toml
wgcf generate                   # 生成 wgcf-profile.conf

# 3) 在 profile 末尾追加本地 SOCKS5 段（最小配置模板）
cat >> "$HOME/.config/warp/wgcf-profile.conf" <<'EOF'

[Socks5]
BindAddress = 127.0.0.1:25344
EOF

# 4) 安装并启动 wireproxy（需较新的 Go 工具链；Ubuntu 22.04 的 apt golang 偏旧，
#    建议用 https://go.dev/dl/ 的官方 tarball 安装，版本以仓库 go.mod 要求为准）
go install github.com/windtf/wireproxy/cmd/wireproxy@v1.1.3
export PATH="$HOME/go/bin:$PATH"
wireproxy -c "$HOME/.config/warp/wgcf-profile.conf"

# 5) 自检：经该出口访问，cloudflare trace 应显示 warp=on
curl -sS --socks5-hostname 127.0.0.1:25344 https://www.cloudflare.com/cdn-cgi/trace | head
```

代理池条目写 `socks5://127.0.0.1:25344`（WARP 端解析域名）。生成后的
`wgcf-profile.conf` 形状如下（占位符，实际值由 `wgcf generate` 写入；**不要手抄真实私钥**）：

```ini
[Interface]
PrivateKey = <warp-private-key>
Address = <warp-address>/32
DNS = 1.1.1.1

[Peer]
PublicKey = <warp-peer-public-key>
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = engage.cloudflareclient.com:2408

[Socks5]
BindAddress = 127.0.0.1:25344
```

注意与限制：

- `wgcf-account.toml` / `wgcf-profile.conf` 含 WARP 私钥/设备令牌，**保持 `0600`、
  不得提交仓库**。
- 需放行出站 UDP 2408（WARP endpoint）；公司/校园网可能封 UDP。
- **Cloudflare WARP 的 IPv4 通常每台主机只有一个共享 `/32` 出口**，且该地址可能已被
  他人耗尽；在同一主机多次 `wgcf register` 通常不会得到多个独立 IPv4 出口。
  要获得多个独立出口，需要不同 VPS、住宅代理，或**可达的 IPv6 `/64`**
  （每条地址是独立出口桶）。WARP 一般只能算"额外 1 个出口"。

可把 wireproxy 交给 systemd 用户服务托管（示例，路径按实际）：

```ini
# ~/.config/systemd/user/wireproxy.service
[Unit]
Description=wireproxy (WARP local SOCKS5)
After=network-online.target

[Service]
ExecStart=%h/.local/bin/wireproxy -c %h/.config/warp/wgcf-profile.conf
Restart=on-failure

[Install]
WantedBy=default.target
```

`systemctl --user enable --now wireproxy`

### 5.6 选择与轮换语义

- **按会话粘性**：会话 seed（对话首条用户消息）哈希到稳定池索引，同一对话的后续轮次
  保持同一出口，避免上游会话/准入状态错位。
- **失败轮换**：429 或隧道失败时换下一个出口；单请求最多尝试
  `min(池长, OMNIROUTE_OPENCODE_FREE_MAX_PROXY_EXITS)` 个出口（默认 4）。
- **隧道失败不计费**：代理不可达 / CONNECT 被拒 / SOCKS 协商失败会把该出口短冷却
  `OMNIROUTE_OPENCODE_FREE_PROXY_COOLDOWN_SECS`（默认 30 秒）、退还本地预算并轮换。
- `https://` 代理 URL 能解析但**不受支持**（本传输不对代理做 TLS）：会在拨号时报
  `https:// proxies are not supported by this transport`，请改用 `http://` 或 `socks5(h)://`。
- 池内无效/重复条目会被丢弃并在日志告警（`ignoring an invalid proxy pool entry`），
  不会让整个池失效；但写错 scheme/端口会导致该出口不可用。

## 6. 预算、会话与回退旋钮

全部旋钮都有默认值，以下为最常用项（完整列表见主 `README.md` 的 Environment variables 表）。
优先级：**环境变量（`OMNIROUTE_` 前缀或裸名，前缀优先）> `[opencode_free]` TOML > 默认值**。

| 环境变量 | 默认 | 含义 |
|---|---|---|
| `OMNIROUTE_OPENCODE_FREE_CREDENTIAL_MODE` | `auto` | 凭据模式：`auto` 轮询托管账号并回退匿名 `public`；`key` 另允许 env/config key；`public` 强制匿名 |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PER_ACCOUNT` | `200` | 每个托管账号的本地 UTC 日请求上限 |
| `OMNIROUTE_OPENCODE_FREE_DAILY_LIMIT_PUBLIC` | `200` | 匿名 `public` 凭据的本地 UTC 日上限（也用于代理池每个出口的初始上限） |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT` | `true` | 从真实日额度 429 学习保守上限（`learned_limit = min(learned, used_at_429)`，跨日保留） |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_LIMIT_TTL_DAYS` | `7` | 多少天没有新 429 后，学到的上限按周期回升 20%（`0` 关闭） |
| `OMNIROUTE_OPENCODE_FREE_LEARNED_MIN_SAMPLES` | `10` | 已接受请求数少于此值时收到的日额度 429，按疑似会话准入短冷却处理（`0` 关闭） |
| `OMNIROUTE_OPENCODE_FREE_SHORT_QUOTA_COOLDOWN_SECS` | `60` | 疑似会话准入拒绝的短冷却秒数（不超过上游给出的 `Retry-After`） |
| `OMNIROUTE_OPENCODE_FREE_MAX_IN_FLIGHT_PER_ACCOUNT` | `2` | 单凭据并发上限，超出则轮换/短暂等待（`0` 不限） |
| `OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK` | `false` | 新会话被长 `Retry-After` 拒绝时，用 warm 会话原 body 重试一次（上游行为验证后再开） |
| `OMNIROUTE_OPENCODE_FREE_SESSION_MEMO_FILE` | `opencode-free-sessions.json` | 会话 memo / warm 集合 / 准入状态文件名（数据目录内） |
| `OMNIROUTE_OPENCODE_FREE_PROXY_POOL` | 未设 | 出口代理池（逗号分隔），未设则直连 |
| `OMNIROUTE_OPENCODE_FREE_MAX_PROXY_EXITS` | `4` | 单请求最多尝试的出口数（同时受池长限制） |
| `OMNIROUTE_OPENCODE_FREE_PROXY_COOLDOWN_SECS` | `30` | 隧道失败出口的短冷却秒数 |
| `OMNIROUTE_OPENCODE_FREE_USE_KEY` | — | 旧开关兼容别名：`true` → `CREDENTIAL_MODE=key`，`false` → `public` |

**首次部署建议**：把 `DAILY_LIMIT_PER_ACCOUNT` / `DAILY_LIMIT_PUBLIC` 调高（如 `10000`），
让 `learned_limit` 从真实 429 学习真实上限。调高本地上限**不会**让上游多给额度，
只是避免网关在真 429 之前就自我限流；学到真实上限后会自动收紧。

状态与持久化：

- `$DATA_DIR/opencode-free-budget.json`：UTC 日预算、learned 上限、egress scope、
  每个出口的计数/冷却。进程每几秒落盘，重启不丢当前窗口。
- `$DATA_DIR/opencode-free-sessions.json`：会话 memo（只存 seed 的 SHA-256 摘要，不存原文）、
  最近 8 个 warm 会话、会话准入状态。
- 两个文件都由网关**自动原子写入并置 `0600`**；**删除即重置**（预算、会话、准入状态全部
  重来；不会影响上游账号本身）。升级/排障时可安全备份后删除。
- 停机重启后继续复用已接受的 `ses_*` 会话，不再为每个对话新造会话——这是避免
  "重启后一片 429"的关键，删除 `opencode-free-sessions.json` 会放弃这一优势。

## 7. 验证清单

按顺序做，任一失败先看 §8。

```bash
# 1) 存活
curl -fsS http://127.0.0.1:20128/healthz                 # → ok

# 2) 免费模型在 /v1/models 里带 "free": true
curl -sS http://127.0.0.1:20128/v1/models \
  | jq -r '.data[] | select(.free) | .id'

# 3) 真实流式请求（模型 id 以第 2 步输出为准；设了 OMNIROUTE_API_KEY 时加
#    -H 'Authorization: Bearer <api-key>'；直连 429 时配置代理池后复测）
curl -sS -N -i --max-time 120 http://127.0.0.1:20128/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"opencode/flint-40b-free","stream":true,"messages":[{"role":"user","content":"Reply with the word: pong"}]}' \
  | head -c 800
# 期望 HTTP 200 + SSE "data:" 行；若 429，记录 body 里的 code 与 Retry-After，按 §8 判断

# 4) 管理态：先登录拿仪表盘会话（密码为 §3 设置的）
TOKEN=$(curl -fsS -X POST http://127.0.0.1:20128/v1/auth/login \
  -H 'Content-Type: application/json' -d '{"password":"<dashboard-password>"}' | jq -r .token)

curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:20128/v1/opencode-free \
  | jq '{credentialMode, bucketScope, sessionAdmissionSuspected,
         sessionAdmissionBlocked, shortQuotaRefusals,
         egress: {poolSize: .egress.poolSize, maxExitsPerRequest: .egress.maxExitsPerRequest,
                  exits: [.egress.exits[] | {label, mode, source, used, limit, healthy}]},
         budget: {dailyLimitPublic: .budget.dailyLimitPublic, scope: .budget.scope,
                  credentials: [.budget.credentials[] | {label, anonymous, used, limit, learnedLimit, exhausted}]}}'
# 关注：credentialMode、bucketScope(unknown/account/egress)、sessionAdmissionSuspected/
#       shortQuotaRefusals/sessionAdmissionBlocked、egress.exits[]（脱敏 label、mode、
#       used/limit、healthy）、budget.credentials[]

# 5) 日志：免费 lane / 出口 / 429 / panic
journalctl --user -u omniroute-rust --since '15 min ago' \
  | grep -Ei 'free.?lane|egress|429|panic'
# system 服务用：journalctl -u omniroute-rust --since '15 min ago' | grep -Ei ...

# 6) （可选）强制重读免费名册
curl -sS -X POST -H "Authorization: Bearer $TOKEN" \
  http://127.0.0.1:20128/v1/opencode-free/refresh | jq '.status, (.exposed | length)'
```

`egress.exits[]` 里 `source` 为 `pool`（匿名池）/ `account`（账号自己的 proxyUrl）/
`observed` / `direct`；`mode` 为 `direct` / `http_connect` / `socks5` / `socks5h`；
`healthy=false` 或 `cooldownUntilMs>now` 表示该出口正在冷却。

## 8. 故障排查

### 8.1 429 的几种含义

| 现象 | 含义 | 动作 |
|---|---|---|
| 429 很快返回（`Retry-After` ≤ 300 秒），body 不一定有 `FreeUsageLimitError` | 突发限流（分钟级窗口） | 网关自动按上游给的时长短冷却；稍后重试即可 |
| 429 `FreeUsageLimitError`，`Retry-After` 指向下一个 00:00 UTC 附近（长） | 该出口的 UTC 日额度耗尽 | 换出口（§5）/ 等 UTC 0 点；查 `budget.credentials[]` 的 `used/learnedLimit/cooldownUntilIso` |
| 429 只发生在**新会话**，旧（warm）会话仍 200；`Retry-After` 却很长 | 上游**会话准入**拒绝（不是请求额度） | 网关默认按疑似准入短冷却（`sessionAdmissionSuspected=true`）；恢复后用 warm 会话继续；验证后可以考虑 `OMNIROUTE_OPENCODE_FREE_SESSION_FALLBACK=true` |
| 403 `FreeTierError` / "只允许官方客户端"类 gate 拒绝（网关对外转成 `503 free_lane_unavailable`） | 上游 agent-shape / 指纹门禁 | 网关已自动满足（强制流式、官方 prompt、官方 `read` 工具、Bun TLS 指纹）；若仍出现，检查是否绕过网关直连上游，或中间代理做了 TLS 中间人 |
| `429 free_lane_budget`（message 含 "skipped locally … No upstream request was sent"） | **本地**预算跳过：所有凭据/出口都在冷却或耗尽 | 不是上游拒绝；看 `/v1/opencode-free` 的冷却与 reset，或临时调高日上限；等待冷却/UTC 重置 |
| 日额度 429 在两个不同账号上 60 秒内先后出现 | 额度按出口共享（egress-scoped） | `bucketScope` 变 `egress`，本轮停止逐账号试探；换出口是唯一有效扩容 |

### 8.2 其他常见问题

| 现象 | 原因 | 动作 |
|---|---|---|
| `503 free_lane_egress`（`code: free_lane_egress`） | 代理不可达 / 407 认证失败 / CONNECT 被拒 / SOCKS 协商失败 / TLS 握手超时 | `journalctl` 里看 `egress failed` 的 `msg` 与脱敏出口；用 `curl -x` 或 `--socks5-hostname` 手工复测；确认 scheme、端口、userinfo；该出口会被短冷却 30 秒后自动轮换 |
| `503 free_lane_unavailable` | 上游 gate 拒绝该请求形状/区域（`403 FreeTierError` 等），不是 401/403，也不是你的 key 问题 | 稍后重试或换 keyed 模型；确认 `baseUrl` 未被改成非官方地址 |
| 代理超时 | 建连/CONNECT/SOCKS 握手有 15 秒预算（`HANDSHAKE_TIMEOUT`），黑洞代理不会拖满整个请求超时 | 检查代理连通性与防火墙；即使加大 `REQUEST_TIMEOUT_MS` 也无益于握手，握手上限仍是 15 秒 |
| `Proxy authentication required` / 407 | 代理池 URL 缺 userinfo，或代理端（tinyproxy `BasicAuth`）未配置认证 | 补 `http://user:pass@host:port`；口令中特殊字符用 percent-encoding（`@`→`%40`） |
| `https://` 代理报 "not supported" | 本传输不对代理做 TLS | 只用 `http://`（CONNECT）或 `socks5(s)://` |
| socks5 / socks5h 连不通 | 语义与 curl 相反 | `socks5://`=代理端解析，`socks5h://`=网关本地解析；端口必填 |
| 首个免费请求慢 | BoringSSL 首次握手 + 名册发现 + 会话建立 | 正常；预热见 §4.1；后续请求会明显变快 |
| 仪表盘 Provider 的 Test 按钮结果仍像直连/失败 | **已知限制**：Test 探测只使用该 connection 自己的 `proxyUrl`，不使用匿名免费 lane 的代理池 | 验证免费 lane 出口请用真实 chat 请求 + `GET /v1/opencode-free` 的 `egress.exits` |
| 代理池配了但 `poolSize=0` | env 拼写错误、TOML 层级不对、条目被校验丢弃、或改完未重启 | 查日志 `ignoring an invalid proxy pool entry`；env 用 `OMNIROUTE_OPENCODE_FREE_PROXY_POOL`（逗号分隔）；TOML 在 `[opencode_free]` 下；`systemctl --user restart` |
| 免费模型名 404 / 不在 `/v1/models` | 名册已刷新掉该 id，或被判定为区域封锁/下架 | 以 `/v1/models` 输出为准；`POST /v1/opencode-free/refresh` 重读 |
| `version 'GLIBC_2.xx' not found` | 构建机 glibc 高于运行机 | 在目标同版本或更老的发行版上构建（§1.6） |
| `libstdc++.so.6: cannot open shared object file` | 运行机缺 C++ 运行库 | `sudo apt install -y libstdc++6` |

**日志关键字**：默认 `RUST_LOG=info`。免费 lane 的关键日志都带 `opencode free lane:` 前缀，
例如 `quota 429; cooling this credential`、`low-usage 429 — suspected session admission`、
`egress failed; rotating to the next exit`、`every attempted proxy exit failed`、
`egress-scoped … parking every credential`。需要更细时临时 `Environment=RUST_LOG=debug`
（日志量较大，排障后记得改回）。

## 9. 升级、配置变更与回滚

### 9.1 备份与升级

```bash
# 备份二进制 + 数据目录（数据目录含凭据，压缩包 0600，放仓库外）
stamp=$(date +%Y%m%d-%H%M)
backup="$HOME/omniroute-backup-$stamp"
install -d -m 700 "$backup"
install -m 755 "$HOME/.local/bin/omniroute" "$backup/omniroute"
tar -C "$HOME/.omniroute-rust" -czf "$backup/data.tgz" .
chmod 600 "$backup/data.tgz"

# 升级二进制（拉源码或从构建机拷贝均可）
git pull --ff-only
cargo build --release
install -m 755 target/release/omniroute "$HOME/.local/bin/omniroute"
systemctl --user restart omniroute-rust
systemctl --user status omniroute-rust --no-pager
curl -fsS http://127.0.0.1:20128/healthz
```

- 升级后建议复查一次 `GET /v1/opencode-free` 字段与一次免费 lane 请求。
- 只换二进制不必停服务；`restart` 会短暂中断请求。
- 数据文件为 JSON，向前兼容；如需完全干净的状态，可备份后删除
  `opencode-free-budget.json` / `opencode-free-sessions.json`（会重置预算与会话记忆）。
- WSL/system 用户的 data dir/file 权限在升级后保持不变；`0600` 不应被破坏。

### 9.2 用 EnvironmentFile 管理免费 lane 配置（推荐）

仓库提供 `deploy/opencode-free.env.example`（全占位符、注释齐全）。安装为 `0600` 后，
用 systemd drop-in 引入：

```bash
install -m 600 deploy/opencode-free.env.example "$HOME/.omniroute-rust/opencode-free.env"
$EDITOR "$HOME/.omniroute-rust/opencode-free.env"        # 填 <user>/<pass>/<proxy-host> 等

mkdir -p "$HOME/.config/systemd/user/omniroute-rust.service.d"
cat > "$HOME/.config/systemd/user/omniroute-rust.service.d/10-opencode-free.conf" <<'EOF'
[Service]
EnvironmentFile=-%h/.omniroute-rust/opencode-free.env
EOF

systemctl --user daemon-reload
systemctl --user restart omniroute-rust
```

- `EnvironmentFile` 每行 `KEY=VALUE`；`#` 整行注释；**不支持** shell 变量展开/命令替换/
  行内注释；文件必须对服务用户可读，含口令时 `0600`。
- system 服务把路径改为绝对路径（如 `/etc/omniroute-rust/opencode-free.env`），属主/权限
  需让服务用户可读。
- 同一台机器尽量只用一种来源（env 或 TOML）；env 优先，混用容易误判生效值。

### 9.3 回滚

```bash
systemctl --user stop omniroute-rust
install -m 755 "$HOME/omniroute-backup-<stamp>/omniroute" "$HOME/.local/bin/omniroute"
# 若数据也需要回滚（注意覆盖会丢失新数据）：
#   rm -rf "$HOME/.omniroute-rust" && mkdir -m 700 "$HOME/.omniroute-rust"
#   tar -C "$HOME/.omniroute-rust" -xzf "$HOME/omniroute-backup-<stamp>/data.tgz"
systemctl --user start omniroute-rust
curl -fsS http://127.0.0.1:20128/healthz
```

回滚后检查 `journalctl --user -u omniroute-rust -n 50` 是否有配置解析错误；
system 服务把 `--user` 换成 `sudo`。

## 10. 日常运维命令

```bash
systemctl --user status omniroute-rust          # 状态
journalctl --user -u omniroute-rust -f          # 实时日志
systemctl --user restart omniroute-rust         # 重启
systemctl --user stop omniroute-rust            # 停止
systemctl --user disable --now omniroute-rust   # 取消自启
```

## 11. 配置 provider key（付费/托管 provider）

编辑 `~/.omniroute-rust/provider-credentials.json`（模板已就位），或往服务里加环境变量：

```bash
systemctl --user edit omniroute-rust
# [Service]
# Environment=OPENAI_API_KEY=sk-...
# Environment=OMNIROUTE_API_KEY=sk-master   # 设置后 /v1/* 需要 Bearer 鉴权
# Environment=OMNIROUTE_THINKING_MODE=auto   # 小上下文客户端移除客户端推理字段
# Environment=OMNIROUTE_COMPRESSION=lite     # 同时压缩重复历史
systemctl --user restart omniroute-rust
```

管理端点（`/v1/opencode-free`、provider-connections、日志/统计等）需要**仪表盘登录会话
（`sess_*`）或 `admin` 角色的 API key，或静态 `OMNIROUTE_API_KEY`**。免费 lane 的匿名
`public` 凭据不需要任何 provider key。

## 12. Small-context clients

`/v1/models` 条目按候选模型计算能力。满足 1M 能力的模型会报告
`contextWindow=1000000` 和对应的 `maxTokens`，并保留 `contextLength` 兼容旧
客户端；混合不同窗口的 combo 对外使用最大窗口，实际请求会过滤上下文不足的
候选。图片/PDF 请求会
只选择具备对应能力的 combo 候选，并通过 `supportsVision`、`supportsPdf`、
`modalities` 暴露给客户端；思考等级通过 `reasoningEfforts` 提供。
需要推理等级的 DSH route 应使用
`api: openai-responses`，这样请求进入
`/v1/responses`；`openai-completions` 才使用 `supportsReasoningEffort` 和
`thinkingFormat: openai` compat 配置。
同时，combo 候选对应的 provider connection 也必须设置
`api_type: openai-responses`；否则网关会按 provider 的 OpenAI Chat 协议出站，
请求会落到 `/v1/chat/completions`。

连接的模型同步会同时保存上游返回的上下文窗口、输出上限、输入模态和思考等级；
未返回的字段才使用本地模型规则。
Provider 的 Test 按具体模型的协议构造探测请求，组合 Test 复用同一逻辑；因此
Responses、Chat、Claude 和 Gemini 模型不会再被统一用 Chat 请求探测。

## 13. 模型能力目录（联网缓存）

多数 provider 的 `/models` 只返回模型 id，不带上下文/模态信息，因此模型能力来
自共享目录快照：

- 缓存文件：`~/.omniroute-rust/model-catalog.json`（原子写入）
- 启动时加载缓存，随后由后台任务按 TTL（默认 24h）自动从
  `https://models.dev/api.json` 刷新；失败只记日志，旧缓存继续生效
- 内置 160 行精简快照兜底，保证离线/首次启动也有数据

```bash
# 查看缓存状态
curl -H "Authorization: Bearer <session>" http://127.0.0.1:20128/v1/model-catalog

# 手动立即刷新
curl -X POST -H "Authorization: Bearer <session>" \
  http://127.0.0.1:20128/v1/model-catalog/refresh

# 关闭自动更新 / 改周期 / 换源
systemctl --user edit omniroute-rust
# Environment=OMNIROUTE_MODEL_CATALOG_AUTOUPDATE=false
# Environment=OMNIROUTE_MODEL_CATALOG_TTL_HOURS=6
# Environment=OMNIROUTE_MODEL_CATALOG_URL=https://models.dev/api.json
```

解析优先级：provider `sync-models` 元数据 → 目录快照 → 名称启发式；目录按模型
id 维度解析，同一模型经任何 provider/relay 访问能力一致。

部署后可检查：

```bash
curl -s -H "Authorization: Bearer $OMNIROUTE_API_KEY" \
  http://127.0.0.1:20128/v1/models
```

## 14. 从 Windows 测试（WSL2）

WSL2 本地端口转发默认开启：Windows 里 `curl http://127.0.0.1:20128/healthz`
或浏览器打开 `http://127.0.0.1:20128/dashboard`。
需要局域网访问时把 unit 里 host 改成 `0.0.0.0`
（`Environment=HOST=0.0.0.0`），并注意暴露范围。

## 15. 仪表盘功能（与原版侧边栏 1:1 对齐）

侧边栏与原版 `sections.ts` 完全一致：10 个 section、8 个分组、94 个条目
（Home · OmniProxy · Analytics · Costs · Monitoring · Dev Tools ·
Agentic Features · Other Features · Configuration · Help），内容区全宽无限宽。

共 76 个页面（完整审计见 `docs/PAGES.md` / `docs/zh/PAGES.md`）：

- **34 个真实数据页面**：首页（快速入门 / 提供者拓扑 / 最近请求）·
  Endpoints · API Manager（多密钥 CRUD）· Providers（连接 CRUD + 连通性测试）·
  Combos · Combos Studio · Provider Quota · Quota share · Compression（设置 +
  Caveman/RTK/Headroom/Ultra/Aggressive/Lite 等全部引擎）· Playground（真发请求）·
  Translator · Batch · Traffic inspector · Usage · Combo Health · Utilization ·
  Cache Health · Route tracing · Compression analytics · Provider Stats ·
  Free tiers · Activity · Logs · Log export（CSV/JSON 下载）· Audit log ·
  Health · Runtime · Resilience · Settings·General/Appearance/Sidebar/Security ·
  CLI code（原版 CLI 工具目录静态复刻）。
- **42 个诚实占位页**：原版上游专属模块（agent 舰队 / gamification /
  MCP·A2A·插件运行时 / 成本核算 / 部分设置子页等），Rust 后端尚未移植，
  页面明示差异、零假数据。

默认语言自动跟随浏览器（顶栏可切换 66 语言、原版消息包，选择持久化，回退 en）、深浅主题、Ctrl+K 快速导航；侧边栏底部
是「重启服务 / 停止服务」（`POST /v1/admin/service/{restart,stop}`，需要登录态；
restart 依赖 systemd `Restart=on-failure`，stop 为干净退出）。

## 16. 基准测试注意

做内存/并发基准时**不要占用线上端口**（默认 20128）。用独立端口与独立数据目录：

```bash
OMNIROUTE_DATA_DIR=/tmp/omni-bench-rs OMNIROUTE_API_KEY=bench-master \
OMNIROUTE_REQUESTS_PER_MINUTE=100000 OMNIROUTE_MIN_TIME_BETWEEN_REQUESTS_MS=0 \
OMNIROUTE_CONCURRENT_REQUESTS=128 \
~/.local/bin/omniroute serve --port 20129 &
```

原版侧必须先用会话（`omniroute reset-password` → `POST /api/auth/login`）执行
`PATCH /api/resilience` 提升限流，否则请求队列会主导结果、对比不公平。
完整方法与数据见 `docs/BENCHMARK.md`。