use super::*;

fn mapper() -> AddressMapper {
    AddressMapper::build(
        "moses@126.example",
        "Moses",
        &[
            ("li.ting@corp.example".into(), "李婷".into()),
            ("li.ting.personal@example.com".into(), "李婷".into()),
            ("wang.yu@corp.example".into(), "王宇".into()),
        ],
    )
}

#[test]
fn same_address_maps_to_the_same_code() {
    let m = mapper();
    assert_eq!(
        m.code_for("li.ting@corp.example"),
        m.code_for("LI.TING@CORP.EXAMPLE")
    );
    // Sorted by address: "li.ting.personal@…" < "li.ting@corp…"
    // ('.' < '@'), so the personal address is A, the work one B.
    assert_eq!(
        m.code_for("li.ting.personal@example.com").unwrap().0,
        "发件人A"
    );
    assert_eq!(m.code_for("li.ting@corp.example").unwrap().0, "发件人B");
    assert_eq!(
        m.code_for("li.ting@corp.example").unwrap().1,
        "sender-b@example.invalid"
    );
    assert_ne!(
        m.code_for("li.ting@corp.example"),
        m.code_for("li.ting.personal@example.com")
    );
    // Same contacts in a different order still assign the same
    // codes: the build sorts by address.
    let shuffled = AddressMapper::build(
        "moses@126.example",
        "Moses",
        &[
            ("wang.yu@corp.example".into(), "王宇".into()),
            ("li.ting.personal@example.com".into(), "李婷".into()),
            ("li.ting@corp.example".into(), "李婷".into()),
        ],
    );
    assert_eq!(
        shuffled.code_for("li.ting@corp.example"),
        m.code_for("li.ting@corp.example")
    );
}

#[test]
fn owner_maps_to_the_fixed_code() {
    let m = mapper();
    assert_eq!(m.code_for("moses@126.example").unwrap().0, "我");
    assert_eq!(
        m.code_for("moses@126.example").unwrap().1,
        "me@example.invalid"
    );
    assert_eq!(m.owner_code_name(), "我");
    assert_eq!(m.owner_code_addr(), "me@example.invalid");
}

#[test]
fn forward_hides_addresses_and_names() {
    let m = mapper();
    let text = "李婷 <li.ting@corp.example> 写信给 Moses <moses@126.example>：抄送王宇 wang.yu@corp.example，正文提到 attacker@evil.example。";
    // The sender hint resolves the bare shared name to the work address.
    let f = m.forward_with(text, Some("li.ting@corp.example"));
    assert!(!f.contains("li.ting@corp.example"));
    assert!(!f.contains("moses@126.example"));
    assert!(!f.contains("wang.yu@corp.example"));
    assert!(!f.contains("attacker@evil.example"));
    assert!(!f.contains("李婷") && !f.contains("王宇") && !f.contains("Moses"));
    assert!(f.contains("发件人B <sender-b@example.invalid>"));
    assert!(f.contains("我 <me@example.invalid>"));
    assert!(!m.has_real_address(&f));
    // Idempotent: codes pass through unchanged.
    assert_eq!(m.forward(&f), f);
}

#[test]
fn reverse_restores_real_names_and_addresses() {
    let m = mapper();
    let coded =
        "发件人B <sender-b@example.invalid> 说方案 B 通过了；请我 <me@example.invalid> 确认。";
    let r = m.reverse(coded);
    assert_eq!(
        r,
        "李婷 <li.ting@corp.example> 说方案 B 通过了；请Moses <moses@126.example> 确认。"
    );
    assert_eq!(
        m.reverse(&m.forward("李婷来信：方案 B 通过")),
        "李婷来信：方案 B 通过"
    );
    // A bare 我 is a pronoun, not the owner code — leave it alone.
    assert_eq!(m.reverse("我觉得方案 B 可以"), "我觉得方案 B 可以");
}

#[test]
fn unknown_address_gets_a_stable_fallback_code() {
    let m = mapper();
    let a = m.forward("请联系 stranger@nowhere.example");
    let b = m.forward("请联系 stranger@nowhere.example");
    assert_eq!(a, b, "fallback must be deterministic");
    assert!(!a.contains("stranger@nowhere.example"));
    assert!(a.contains("@example.invalid"));
}

#[test]
fn message_id_in_angle_brackets_survives_forward() {
    let m = mapper();
    // The brief cites mails by message-id; the id looks like an
    // address but is an identifier, and mangling it would corrupt
    // the delivered brief (a hash code cannot be reversed).
    let text = "- [<m201@corp.example>] 方案 B 通过（来源：李婷 <li.ting@corp.example>）";
    let f = m.forward(text);
    assert!(
        f.contains("[<m201@corp.example>]"),
        "message-id must survive: {f}"
    );
    assert!(
        !f.contains("li.ting@corp.example"),
        "the real sender address must still be hidden: {f}"
    );
    assert!(f.contains("sender-b@example.invalid"));
    assert_eq!(m.reverse(&f), text, "round trip must restore the citation");
}

#[test]
fn fallback_code_reverses_within_the_same_mapper() {
    let m = mapper();
    let coded = m.forward("请联系 stranger@nowhere.example，抄送王宇。");
    assert!(coded.contains("@example.invalid"));
    // The model echoes the code; the same mapper restores the
    // original before delivery.
    let echoed = format!("{coded} 已收到。");
    assert_eq!(
        m.reverse(&echoed),
        "请联系 stranger@nowhere.example，抄送王宇。 已收到。"
    );
    // A rebuilt mapper has no record of this instance's
    // fallbacks: the code survives (the hash cannot be inverted),
    // which is why the known set must cover senders that matter.
    let rebuilt = mapper();
    assert!(
        rebuilt.reverse(&coded).contains("@example.invalid"),
        "a rebuilt mapper cannot restore a fallback code"
    );
}
