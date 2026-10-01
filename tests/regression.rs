//! 回归测试：锁定纯函数行为，防止改动引入回归。
//!
//! 这些测试覆盖「无需网络 / Redis」的纯逻辑。断言值均来自对实际实现的观测，
//! 用于在后续重构（尤其是 unwrap 加固）时保证行为不变。

use biliroaming_rust_server::mods::cache::check_ep_available;
use biliroaming_rust_server::mods::tools::{eid_to_mid, gen_aurora_eid};
use biliroaming_rust_server::mods::types::Area;
use serde_json::json;

// ---------------------------------------------------------------- Area

#[test]
fn area_num_roundtrip() {
    for (n, s) in [(1u8, "cn"), (2, "hk"), (3, "tw"), (4, "th")] {
        let a = Area::new(n);
        assert_eq!(a.num(), n, "Area::new({n}).num() 应等于 {n}");
        assert_eq!(a.to_str(), s, "Area::new({n}).to_str() 应为 {s}");
    }
}

#[test]
fn area_new_panics_on_invalid_num() {
    // 记录现状：非法 area_num 会 panic（而非返回错误）。
    // 这是已知的健壮性隐患，调用点目前都保证传入 1..=4。
    let r = std::panic::catch_unwind(|| Area::new(0).num());
    assert!(r.is_err(), "Area::new(0) 当前会 panic");
    let r = std::panic::catch_unwind(|| Area::new(5).num());
    assert!(r.is_err(), "Area::new(5) 当前会 panic");
}

// ------------------------------------------------- check_ep_available

#[test]
fn check_ep_available_known_codes() {
    // 正常
    assert!(check_ep_available(
        &json!({"code": 0, "message": "success"})
    ));
    // 大会员专享限制 -> 视为可用（内容存在，只是要会员）
    assert!(check_ep_available(
        &json!({"code": -10403, "message": "大会员专享限制"})
    ));
    // 平台不可观看 -> 可用
    assert!(check_ep_available(
        &json!({"code": -10403, "message": "抱歉您所使用的平台不可观看！"})
    ));
    // 访问权限不足 -> 可用
    assert!(check_ep_available(
        &json!({"code": 10015002, "message": "访问权限不足"})
    ));
    // -10500（家宽风控）-> 可用
    assert!(check_ep_available(&json!({"code": -10500, "message": "x"})));
}

#[test]
fn check_ep_available_unavailable_cases() {
    // 地区不可观看 -> 不可用
    assert!(!check_ep_available(
        &json!({"code": -10403, "message": "抱歉您所在地区不可观看！"})
    ));
    // -404 -> 不可用
    assert!(!check_ep_available(
        &json!({"code": -404, "message": "啥都木有"})
    ));
    // 未知码 -> 不可用
    assert!(!check_ep_available(&json!({"code": 999, "message": "?"})));
    // 字段缺失不应 panic（回归保护：此前的 unwrap 加固）
    assert!(!check_ep_available(&json!({})));
    assert!(!check_ep_available(&json!({"code": "not-a-number"})));
}

// ------------------------------------------------------- aurora eid

#[test]
fn aurora_eid_roundtrip_for_typical_mids() {
    // 典型 mid 长度可无损往返
    for mid in ["114514", "123456789", "208259"] {
        let eid = gen_aurora_eid(mid);
        assert!(!eid.is_empty());
        assert_eq!(
            eid_to_mid(&eid).as_deref(),
            Ok(mid),
            "mid={mid} 经 gen_aurora_eid/eid_to_mid 应可往返"
        );
    }
}

#[test]
fn eid_to_mid_rejects_garbage() {
    // 非 base64 / 非法输入不应 panic，只返回 Err
    assert!(eid_to_mid("!!!not-base64!!!").is_err());
    // 实测：空字符串会返回 Ok("")（base64 空输入解码为零长度，循环不执行）。
    // 记录该现状而非假设其报错；调用方需自行判空。
    assert_eq!(eid_to_mid("").as_deref(), Ok(""));
}

#[test]
fn gen_aurora_eid_is_deterministic() {
    // 同一 mid 必须产生同一 eid（用于缓存与校验）
    let a = gen_aurora_eid("114514");
    let b = gen_aurora_eid("114514");
    assert_eq!(a, b);
}

// ------------------------------------------------------------ 其它

#[test]
fn et_type_serializes_to_valid_json() {
    // 回归保护：EType 的错误响应必须是合法 JSON
    // （此前 UserLoginInvalid 用了双大括号导致非法 JSON）
    use biliroaming_rust_server::mods::types::EType;

    for e in [
        EType::ServerGeneral,
        EType::ReqSignError,
        EType::ReqUAError,
        EType::InvalidReq,
        EType::UserNotLoginedError,
        EType::UserLoginInvalid,
        EType::UserNonVIPError,
        EType::UserWhitelistedError,
        EType::ServerOnlyVIPError,
        EType::ServerFatalError,
        EType::UserBlacklistedError(0),
        EType::UserBlacklistedError(1800000000),
    ] {
        let s = e.to_string();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&s);
        assert!(parsed.is_ok(), "EType 响应必须是合法 JSON，实际: {s}");
        let v = parsed.unwrap();
        assert!(v.get("code").is_some(), "响应应含 code 字段: {s}");
    }
}
