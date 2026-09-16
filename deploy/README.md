# omniroute-rust 部署（WSL systemd 用户服务）

服务已在部署时写入 `$HOME`：

- 二进制：`~/.local/bin/omniroute`（release 构建）
- 数据目录：`~/.omniroute-rust/`（provider-credentials.json、.env）
- 服务单元：`~/.config/systemd/user/omniroute-rust.service`（`deploy/omniroute-rust.service` 为仓库内副本）

## 常用命令

```bash
systemctl --user status omniroute-rust          # 状态
journalctl --user -u omniroute-rust -f          # 实时日志
systemctl --user restart omniroute-rust         # 重启
systemctl --user stop omniroute-rust            # 停止
systemctl --user disable --now omniroute-rust   # 取消自启
```

## 换二进制（重新部署）

```bash
cargo build --release
install -m 755 target/release/omniroute ~/.local/bin/omniroute
systemctl --user restart omniroute-rust
```

## 配置 provider key

编辑 `~/.omniroute-rust/provider-credentials.json`（模板已就位），或往服务里加环境变量：

```bash
systemctl --user edit omniroute-rust
# [Service]
# Environment=OPENAI_API_KEY=sk-...
# Environment=OMNIROUTE_API_KEY=sk-master   # 设置后 /v1/* 需要 Bearer 鉴权
systemctl --user restart omniroute-rust
```

## 首次登录

首次部署后管理员密码默认为 **CHANGEME**（与原版一致）。打开
`/dashboard` 会显示密码输入框，也可以在仪表盘顶部横幅里立即改密。

```bash
# 服务日志里也会提醒
journalctl --user -u omniroute-rust -n 10 | grep CHANGEME
```

改密方式（三选一）：
1. 仪表盘顶部横幅 → 输入新密码 → change now
2. `curl -X POST http://127.0.0.1:20128/v1/auth/change-password -H ... -d '{"current_password":"CHANGEME","new_password":"..."}'`
3. 下次启动带环境变量：`systemctl --user edit omniroute-rust` 加
   `Environment=OMNIROUTE_ADMIN_PASSWORD=你的密码` 并 restart

# 从 Windows 测试

WSL2 本地端口转发默认开启：Windows 里 `curl http://127.0.0.1:20128/healthz`
或浏览器打开 `http://127.0.0.1:20128/dashboard`。
需要局域网访问时把 unit 里 host 改成 `0.0.0.0`
（`Environment=HOST=0.0.0.0`），并注意暴露范围。

## 管理员密码存放位置与找回

| 项 | 位置 |
|---|---|
| 密码哈希（加盐 SHA-256，无明文，权限 600） | `~/.omniroute-rust/dashboard-auth.json` |
| 部署级覆盖（每次启动生效） | systemd 单元里的 `Environment=OMNIROUTE_ADMIN_PASSWORD=...` |
| 首次安装默认值 | `CHANGEME` |

改密码：仪表盘顶部横幅 / 设置·安全页，或 API `POST /v1/auth/change-password`。

**忘了密码 / 改完登不上时重置**（服务运行时也可，立即生效）：

```bash
omniroute reset-password --password '你的新密码'      # 至少 8 位
printf '新密码\n新密码\n' | omniroute reset-password   # 管道模式（第一行密码，第二行确认）
omniroute reset-password --password-stdin <<< '新密码'  # 整段 stdin 作为密码
```

重置写的是 `$DATA_DIR/dashboard-auth.json`，网关会热读取该文件，无需重启。

> 安全提醒：若仪表盘端口对公网开放（如 0.0.0.0:20128），务必先改掉默认的 `CHANGEME`。
