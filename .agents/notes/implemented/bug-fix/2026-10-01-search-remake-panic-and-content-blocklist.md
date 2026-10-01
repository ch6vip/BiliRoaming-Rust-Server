# Agent Note: 搜索重写配置 panic 与内容屏蔽

Status: implemented

## Problem

对照参考实现 `bili-vd-bak/biliroaming-ts-server-vercel` 逐文件审查后，发现本仓库两处差异：

1. `handler.rs` 在插入搜索替换条目时使用 `serde_json::from_str(&search_remake_date).unwrap()`（共 4 处）。`search_remake_date` 来自配置 `appsearch_remake` / `websearch_remake`（`HashMap<String, String>`），运维填错一个字符即让请求线程 panic。这与第一轮修复的 panic 属同一类，当时遗漏了此处。
2. 缺少按内容屏蔽的能力。参考实现有 `block_bangumi`（ep/cid/avid/bvid），本仓库只有按 UID 的黑白名单。

## Decision

- 抽出 `parse_search_remake(config, is_app, host) -> Result<Option<Value>, String>`，把解析错误变成可上报的值；调用点记录日志并回退到上游原始响应。抽出函数是为了让行为可被单元测试直接覆盖（HTTP 层测试会先被签名校验拦下，无法到达该分支）。
- 新增四个配置项 `block_bangumi_ep` / `block_bangumi_cid` / `block_bangumi_avid` / `block_bangumi_bvid`，全部 `#[serde(default)]` 为空，保持现有配置兼容。命中即在上游调用前返回 `-10403`。
- 抽出 `blocked_content(query, config) -> Option<String>`，返回命中的参数名便于日志定位；非数字 id 与空 bvid 一律视为不匹配，不引入新的 panic 面。
- 未采纳参考实现的 `try_unblock_CDN_speed`（改写 `bw=`，有效性未验证）、PG/Notion 黑白名单、以及其 admin 接口。

## Alternatives considered

1. 沿用内联 `unwrap()` 并只加配置校验：校验只在启动时跑一次，运行时改配置或迁移路径仍可能绕过，且无法测试。
2. 用 `unwrap_or_default()` 静默忽略非法配置：会掩盖配置错误，运维无法得知替换失效；选择显式报错 + 日志。
3. 内容屏蔽放在 UID 黑白名单里一起判断：两者语义不同（一个是内容、一个是用户），且内容屏蔽应在任何上游调用前生效以省流量。
4. 照搬参考实现的 gRPC 透传：见下。

## Consequences

- 配置错误的后果从"服务崩溃"变为"该 host 的搜索替换静默失效并留下 error 日志"，行为可预期。
- 内容屏蔽是拒绝名单，不是授权名单；配置为空时不改变任何现有行为。
- 新增 `EType::ContentBlockedError` 变体，返回 `-10403`，与黑名单错误码一致，客户端无需改动。

## gRPC 路径的结论（未实现）

参考实现在 Next.js `rewrites` 中把 `bilibili.app.playurl.v1.PlayURL`、`bilibili.pgc.gateway.player.v1.PlayURL`、`bilibili.community.service.dm.v1.DM` 透传到上游。本项目**无法照搬**：

- actix-web 4.15 与 actix-http 3.18.12 的 h2 层没有任何发送 trailers 的 API（全库检索 `trailers` 仅命中 h1 chunked 解析与 `TE: trailers` 头常量），而 gRPC 必须用 trailer 帧回传 `grpc-status`。
- 参考实现是纯透传，不带 resign 与区域代理，因此它只让新客户端不报错，并不解锁。

替代方案是在 Nginx/Caddy 层转发这三条路径。本次未实现、未验证，已记入 `DEPLOY.md` 第 10.2 节。

## Verification

`cargo test --locked` 30 项通过；`cargo clippy --locked --all-targets` 与 `cargo fmt --all -- --check` 通过；`scripts/audit-dependencies.py` 319 包、未解决项 0。

新增两条测试均做变异校验：
- `malformed_search_remake_entry_is_reported_not_panicking`：把实现改回 `unwrap()` 后测试确实 panic 失败。
- `content_blocklist_matches_configured_ids`：覆盖四个列表命中、未列出的 id、非数字 id、空 bvid 与无关参数。

第一版测试曾用 HTTP 层断言，变异后仍通过（请求先被签名校验拦下，未到达目标分支），因此改为直接测试抽出的纯函数。
