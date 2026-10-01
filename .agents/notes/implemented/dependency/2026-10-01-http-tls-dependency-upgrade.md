# Agent Note: HTTP/TLS 依赖升级与 h2 安全回移

Status: implemented

## Problem

行为修复完成后，OSV 扫描仍报告旧版 HTTP/TLS 依赖告警：`rustls 0.20`、`rustls-webpki 0.101`、`ring 0.16`、`rust-crypto 0.2`、`rustc-serialize`、`time 0.1`，以及 `h2 0.3.27` 的拒绝服务告警 `RUSTSEC-2026-0258` / `GHSA-q83h-524g-xf6h`。这些是构建期依赖，不能靠应用层输入校验消除。

`h2` 的官方修复落在 0.4.16，但 Actix HTTP 3.18.12 依赖 `http 0.2` 与 `h2 0.3`，两者接口不兼容，无法仅靠升级版本消除告警。

## Decision

- 迁移可升级的部分：`reqwest 0.12`（`rustls-tls` + `http2`）、`rustls 0.23`、`rustls-pki-types 1.x`、`actix-web 4.15`、`actix-governor 0.10`，移除停维的 `rust-crypto`，用 `md5` crate 保留上游要求的 MD5 签名协议。
- `h2` 采用 `[patch.crates-io]` 指向 `vendor/h2`，在 0.3.27 接口上回移官方修复：256 字节开销阈值、25,600 字节连接预算、消费 DATA 后归还预算、丢弃非末尾空 DATA、超预算时以 `ENHANCE_YOUR_CALM` 关闭连接。
- 回移保持 h2 0.3 的流控口径（解码后载荷长度），不引入 0.4 中与本告警无关的填充记账改动。
- 审计脚本把该回移单列为 `verified_backports`，其他任何未解决告警都让脚本非零退出，不静默忽略。
- 补丁范围固定：只改 `src/` 下 4 个文件，来源与摘要分别记录在 `vendor/h2/SECURITY-PATCH.md` 与 `vendor/h2/backport.json`。

## Alternatives considered

1. 强行把 h2 升到 0.4：需要连带升级 Actix 的 HTTP 类型，等于替换网络栈；在只做安全加固的范围内风险过高。
2. 关闭 HTTP/2 只留 HTTP/1.1：能绕过该告警，但会改变对外协议能力，属于用降级掩盖问题。
3. 在应用层限制请求体大小：无法阻止连接级 DATA 帧洪水，因为攻击发生在帧解析阶段，早于任何应用逻辑。
4. 忽略该告警并在文档里说明：把风险留给运维，且审计脚本会长期无法区分"已知"与"新增"。

## Consequences

- 回移必须在 Actix 采用已修复 h2 后删除，`Cargo.toml` 中的注释与 `SECURITY-PATCH.md` 都记录了这一点。
- 修改 vendored 源码会使摘要失配，审计脚本按设计拒绝继续，必须显式复核并更新摘要，避免补丁被静默替换。
- 协议行为在真实帧层面有测试覆盖：空帧、小帧、带填充帧洪水被拒绝，正常小数据流不受影响。
- 依赖升级改变了 TLS 后端（ring 版本），部署时需确认证书链与私钥格式仍可加载。

## Verification

`cargo test --locked`、`cargo clippy --locked --all-targets`、`cargo fmt --all -- --check` 均通过；`python3 scripts/audit-dependencies.py` 报告 319 个包、未解决项 0、h2 回移已归档。协议测试位于 `tests/network_security.rs`，其中"未被读取的请求体归还帧预算"用例经过变异校验：移除预算归还后该用例确实失败并报出 `ENHANCE_YOUR_CALM` GOAWAY。
