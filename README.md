# rust-otel-starter

Rust 微服务脚手架：Axum + SQLite + OpenTelemetry 上报，开箱即用。

## 先说结论：Cloudflare 能不能当免费 OTEL 后端？

**不能。** Cloudflare 没有提供给外部服务上报的 OTLP 接收端点，方向是反的：

- Cloudflare Workers Observability（traces/logs）可以把 **Worker 自己的** traces 和 logs 用 OTLP **导出到第三方**后端（SigNoz、Grafana Cloud、Honeycomb 等），目前 beta；Workers Free 套餐不可用，Paid 套餐每月含 1000 万事件，超出 $0.05/百万事件，beta 期内免费，2026 年 3 月 1 日起计费。Metrics 导出还不支持。
- 你的本地服务没法直接把 span 发给 Cloudflare，因为没有可用的接收地址。

想在 Cloudflare 上硬做也不是完全没路：可以自己写一个 Worker 当 OTLP receiver（Cloudflare 文档里对自定义接收端的要求就是：接受 OTLP/HTTP JSON，`/v1/traces` 和 `/v1/logs` 两个端点），再把 span 存进 R2 / D1 / Analytics Engine，自己搭查询界面。但那等于自己造一个简陋版 SigNoz，只适合玩，不适合当答案。

## 那 SigNoz 怎么办？

SigNoz 是你最喜欢的，但：

- **SigNoz Cloud 没有免费套餐**，只有 30 天试用；之后 Teams $49/月起（含 $49 用量，logs/traces $0.30/GB，15 天保留）。
- 自建社区版免费，但你没有合适的服务器。

所以这个 demo 的 OTLP 端点做成环境变量可配，默认推荐 **Grafana Cloud 免费版**（永久免费，不用信用卡）：每月 10,000 active series + 50 GB logs + **50 GB traces**，14 天保留，足够做 demo。等你有服务器了，把 endpoint 一换就能连回自建 SigNoz，代码不用改。

## 环境要求

- Rust 工具链（`cargo`；本机用 Rust 1.100 验证过）
- 不需要数据库服务：默认内存 SQLite；不需要 OTEL 后端也能跑（不填 endpoint 就只看本地日志）
- Docker（可选，用来跑容器版）

## 跑起来

```sh
cp .env.example .env   # 按里面注释填后端地址和 key；不填也能跑，只是不上报
cargo run
```

再开一个终端发请求：

```sh
curl localhost:3000/health
curl localhost:3000/users            # 种子数据：3 个用户
curl localhost:3000/users/<id>      # id 是上面返回的 uuid
curl -X POST localhost:3000/users -H 'Content-Type: application/json' \
  -d '{"name":"Linus Torvalds","email":"linus@example.com"}'
curl localhost:3000/demo/fanout      # 扇出 + trace 透传：一次请求产生完整链路
```

去后端（Grafana Cloud Explore > Tempo / SigNoz Traces）搜 `service.name = rust-otel-starter`，能看到：

- `GET /users`、`POST /users`、`GET /demo/fanout` 的 handler span
- handler 下的 `db.query` 子 span（SQLite 查询）
- `/demo/fanout` 里 `http.client` 调 `/health` 的 client span，以及它在服务端产生的子 span（traceparent 透传的证明）
- 指标 `http_requests_total`（按 route 和 status 打标签，10 秒上报一次）

## 数据

- 数据库用 SQLite，默认内存库（`sqlite::memory:?cache=shared`），**不持久化**，重启就没了；想落盘把 `DATABASE_URL` 改成 `sqlite:demo.db`。
- 后续计划：数据库迁到 Neon Postgres，`DATABASE_URL` 改成 Postgres 连接串即可（migration/seed 需换成 Postgres 方言）。
- `migrations/20251125000000_create_users.sql`：建 `users` 表。
- `seeds/seed_users.sql`：3 条种子数据，启动时表为空才插入。

## 项目结构

- `src/main.rs`：全部代码（路由、数据库、telemetry 初始化、fanout）
- `migrations/`：建表 migration，启动时自动跑
- `seeds/seed_users.sql`：种子数据
- `Cargo.toml` / `Cargo.lock`：依赖
- `Dockerfile`：多阶段构建镜像
- `.env.example`：配置模板，复制成 `.env` 后填值

## 安全

- 真实密钥只放本地 `.env`（已在 `.gitignore` 里，**不要提交**）；`.env.example` 里只有占位符。
- 这个仓库不含任何账号、密码、token 或个人邮箱。

## 配置

| 变量 | 说明 |
| --- | --- |
| `PORT` | 监听端口，默认 3000 |
| `DATABASE_URL` | 默认内存 SQLite |
| `RUST_LOG` | 日志级别，默认 `rust_otel_starter=debug` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP 根地址，如 `https://otlp-gateway-<region>.grafana.net/otlp`（不填 = 不上报） |
| `OTEL_EXPORTER_OTLP_HEADERS` | 逗号分隔的 `k=v`，如 `Authorization=Basic xxx` |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf`（默认）或 `http/json` |

## Docker

```sh
docker build -t rust-otel-starter .
docker run -p 3000:3000 --env-file .env rust-otel-starter
```
