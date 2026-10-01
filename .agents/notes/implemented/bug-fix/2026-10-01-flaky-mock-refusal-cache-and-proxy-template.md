# Agent Note: 偶发测试竞态、拒绝响应缓存与反代示例

Status: implemented

## Problem

上一轮对照参考实现后遗留三项待办：

1. `tests/hardening.rs` 的 `custom_notification_posts_configured_body` 在全量运行中偶发失败（约 10 次 1 次），报 `send_report` 返回 `Err("")`。失败那次耗时 24 秒，而正常为 6 秒。
2. 客户端被拒绝后会立即重试，每次都打到本进程并穿透到上游。参考实现对拒绝响应设了 30 秒缓存，本仓库没有任何响应缓存头。
3. gRPC 路径只能放在反向代理层，但仓库里没有可用的反代配置模板。

## Decision

**偶发测试**：根因是 mock 服务器在 `write_all` 之后直接丢弃 socket。客户端此时可能仍有未读的在途数据，丢弃带未读数据的 socket 会让操作系统发 RST 而非 FIN，客户端侧表现为连接错误。改为写入后 `flush`、显式 `shutdown`（发送 FIN）、再 `read_to_end` 排空在途数据后才释放。顺带补上 chunked 请求体解析（原先只处理 `Content-Length`）。

**拒绝响应缓存**：新增 `build_static_refusal_response!` 宏，附加 `Cache-Control: public, max-age=30`。只用于结果不随配置变化的拒绝——请求格式错误、UA 不合法、签名错误、客户端版本过旧、参数缺失。**明确不用于**黑名单、白名单、内容屏蔽、凭据接口：这些在运维改配置后应立即生效，缓存会让已解除的条目继续拒绝 30 秒。凭据响应保持 `no-store`。

**反代示例**：写入 `DEPLOY.md` 第 3.4 节，含 TLS 终止、`X-Real-IP` 覆盖、gRPC 三条路径的 `grpc_pass`，并显式标注该配置**未在本仓库环境实测**。

## Alternatives considered

1. 偶发测试改用重试或放宽断言：会掩盖真实竞态，且掩盖后无法判断是测试问题还是产品问题；选择修 mock 的 socket 生命周期。
2. 对所有拒绝响应统一加缓存（照搬参考实现）：实现最简单，但会让黑名单/内容屏蔽的改动延迟生效。参考实现自身就有这个副作用。
3. 只在应用层给 gRPC 路径返回 `-404` 而不转发：新客户端仍会报错，等于没解决问题；转发放代理层是唯一可行位置。
4. 缓存时间取更长（如 5 分钟）：拒绝响应里含 `-412 风控` 这类可能很快变化的判定，30 秒是压力与时效的折中。

## Consequences

- mock 服务器现在能正确排空在途数据，不再产生偶发 RST。33 次连续运行（含 16 线程高并发）无失败。
- 静态拒绝响应可被 CDN/客户端缓存 30 秒，显著降低重试风暴。代价是这类拒绝的错误信息变更最多延迟 30 秒生效——它们都是固定文案，无实际影响。
- 缓存策略有回归测试守护：把内容屏蔽误改为可缓存宏时，测试会失败。
- 反代示例未经验证，文档中已明确标注，避免被当作实测结论使用。

## 过程中的一次自伤（已修复）

用 PowerShell 批量替换调用点时，用了 `Get-Content -Raw`（默认按 cp936 解码 UTF-8 文件）加 `WriteAllText`（按 UTF-8 写入），把 `src/mods/handler.rs` 里所有中文变成了乱码。发现后按 `utf8_encode(cp936_decode(...))` 的逆变换还原，8 行在 cp936 解码阶段就已丢失的字符（U+FFFD）逐行从 HEAD 取回。

事后核验：全文件无 U+FFFD，所有非 ASCII 字符均可无损通过 cp936 往返，`HEAD` 中消失的行全部是本次有意替换的行。最终对 15 个改动文件做了一次乱码扫描，全部通过。

**教训**：在 Windows PowerShell 里改 UTF-8 源文件，不能用 `Get-Content -Raw` + `WriteAllText` 做文本替换。要么用 Edit 工具，要么显式指定 `[Text.Encoding]::UTF8`。

## Verification

`cargo test --locked` 31 项通过（2+16+5+8）；`cargo clippy --locked --all-targets`、`cargo fmt --all -- --check`、`scripts/audit-dependencies.py`（319 包，未解决项 0）均通过。

新增测试 `static_refusals_are_cacheable_but_state_dependent_ones_are_not` 做变异校验：把内容屏蔽改用可缓存宏后测试确实失败。

第一版该测试用 app 路径构造内容屏蔽请求，结果先被签名校验拦下（签名门在屏蔽检查之前），断言拿到的是可缓存响应而非 `-10403`。改用 web 路径后正确覆盖了目标分支。
