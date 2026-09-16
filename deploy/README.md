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

## 从 Windows 测试

WSL2 本地端口转发默认开启：Windows 里 `curl http://127.0.0.1:20128/healthz`
或浏览器打开 `http://127.0.0.1:20128/dashboard`。
需要局域网访问时把 unit 里 host 改成 `0.0.0.0`
（`Environment=HOST=0.0.0.0`），并注意暴露范围。
