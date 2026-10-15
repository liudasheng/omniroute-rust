# Runbook：DSH「Connection error.」与 Node Happy Eyeballs

> 现象：DSH Web 会话运行中突然中止，提示「失败原因：Connection error.」，约 4.1s 后重试。
> 适用：本机 WSL（`dsh-web.service`）直连 Cloudflare 系上游（opencode.ai / openrouter 等）。
> 修复日期：2026-10-15　·　根因：Node ≥20 `autoSelectFamily` 尝试预算 < 实际 TCP 连接时延。

## 一、根因（一句话）

Node ≥20 默认开启 Happy Eyeballs（`autoSelectFamily`），单地址尝试预算 **250ms**；本机到
Cloudflare 边缘（opencode.ai）的 TCP 连接实测 **272–390ms**，于是 IPv4 尝试被放弃、回落到
**无路由的 IPv6**（本 WSL 无 IPv6 默认路由，但 DNS 仍返回 4 条 AAAA），最终抛 `ETIMEDOUT`
→ OpenAI SDK 包装为 `APIConnectionError: Connection error.` → DSH 退避重试（多为 4.1s 后成功）。

## 二、判据（如何 30 秒内确认是它）

```bash
# 1) 真实连接时延是否 >250ms
curl -s -o /dev/null -w 'connect=%{time_connect}\n' https://opencode.ai/zen/v1/models

# 2) IPv6 是否根本不可达（本例为 0/10 成功）
curl -s -o /dev/null -m 8 -6 -w 'ipv6=%{http_code}\n' https://ipv6.google.com
ip -6 route show default            # 空 = 无 IPv6 默认路由

# 3) NODE_OPTIONS 矩阵对比失败率（决定性证据）
#    流式 + 空闲复用，与生产同栈；默认 vs 加 flag
NODE_OPTIONS="" node probe.mjs
NODE_OPTIONS="--network-family-autoselection-attempt-timeout=2000" node probe.mjs
```

实测：默认流式 **6/8 失败** → 加 flag **0/8**；非流式 **2/15** → **0/15**。
注意 `--dns-result-order=ipv4first` 单独使用**更糟**（8/15），不要用它顶替本条修复。

## 三、修复

```bash
mkdir -p ~/.config/systemd/user/dsh-web.service.d
cat > ~/.config/systemd/user/dsh-web.service.d/10-node-happy-eyeballs.conf <<'EOF'
[Service]
Environment=NODE_OPTIONS=--network-family-autoselection-attempt-timeout=2000
EOF
systemctl --user daemon-reload
systemctl --user restart dsh-web        # 会中断当前 Web 会话，页面刷新即恢复
```

校验：

```bash
PID=$(pgrep -f "dsh/lib/bin.js web" | head -1)
tr '\0' '\n' < /proc/$PID/environ | grep NODE_OPTIONS   # 应出现该 flag
systemctl --user show dsh-web -p Environment | tr ' ' '\n' | grep NODE_OPTIONS
journalctl --user -u dsh-web --since "-10 min" | grep -ci "connection error"   # 期望 0
```

备选（等效，实测同为 0 失败）：`NODE_OPTIONS=--no-network-family-autoselection`（彻底关闭地址族自选）。

## 四、边界与残留

- 该 flag 是**进程级**的，覆盖 DSH 内所有出网 provider（opencode.ai、openrouter 等），
  对走 `127.0.0.1` 的 `omniroute-rust` provider 无影响。
- `opencode-web.service` 为 ELF 单体二进制（非 Node CLI 入口），本 flag 不适用。
- 真正的网络侧改善（把到 Cloudflare 的连接时延压到 250ms 以下）不在本机可控范围；
  若日后换网络环境致时延继续增大，可把预算提到 3000–5000ms。
- 排查 DSH 报错前先分清 provider 是「直连上游」还是「走本机网关」：
  `grep -n "baseURL\|apiKeyEnv" ~/.dsh/profiles/web/cordis.patch.yml`。
