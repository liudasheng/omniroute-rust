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
# Environment=OMNIROUTE_THINKING_MODE=auto   # 小上下文客户端移除客户端推理字段
# Environment=OMNIROUTE_COMPRESSION=lite     # 同时压缩重复历史
systemctl --user restart omniroute-rust
```

## Small-context clients

`/v1/models` 条目按候选模型计算能力。满足 1M 能力的模型会报告
`contextWindow=1000000` 和对应的 `maxTokens`，并保留 `contextLength` 兼容旧
客户端；混合不同窗口的 combo 对外使用最大窗口，实际请求会过滤上下文不足的
候选。图片/PDF 请求会
只选择具备对应能力的 combo 候选，并通过 `supportsVision`、`supportsPdf`、
`modalities` 暴露给客户端；思考等级通过 `reasoningEfforts` 提供。
需要推理等级的 DSH route 应使用 `api: openai-responses`，这样请求进入
`/v1/responses`；`openai-completions` 才使用 `supportsReasoningEffort` 和
`thinkingFormat: openai` compat 配置。

连接的模型同步会同时保存上游返回的上下文窗口、输出上限、输入模态和思考等级；
未返回的字段才使用本地模型规则。

若使用本地 DSH 的“获取模型”功能，可执行一次：

```bash
node deploy/patch-dsh-auto-reasoning.mjs
systemctl --user restart dsh-web.service
```

补丁会让 DSH 保留 `/models` 返回的 `reasoningEfforts` 和兼容格式，并在采纳
自定义模型时写入 profile；之后新增模型无需手工补齐推理等级。DSH 升级后可安全
重复执行，脚本会检查版本形状并保持幂等。

部署后可检查：

```bash
curl -s -H "Authorization: Bearer $OMNIROUTE_API_KEY" \
  http://127.0.0.1:20128/v1/models
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

## 仪表盘功能（与原版侧边栏 1:1 对齐）

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

## 基准测试注意

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
