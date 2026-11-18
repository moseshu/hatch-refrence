//! Built-in checks never trust a worker's description of the resource.
use crate::executors::{EmailPayload, EventPayload, ExecContext};
use crate::verification::EvidenceCheck;
use hatch_connectors::{CalendarRepo, MailSender};
use hatch_core::{Error, Result};
use serde_json::{json, Value};

pub(crate) fn file(payload: &Value) -> Result<EvidenceCheck> {
    let p: crate::ws_exec::WritePayload = serde_json::from_value(payload.clone())?;
    let ws = crate::Workspace::new(&p.root)?;
    match ws.read_bytes(&p.path, crate::checkpoint::MAX_SNAPSHOT_BYTES)? {
        Some(bytes) => Ok(EvidenceCheck::compare(
            "file-content-v1",
            "workspace_read",
            &ws.resolve(&p.path)?.display().to_string(),
            p.content.as_bytes(),
            &bytes,
        )),
        None => Ok(EvidenceCheck::unknown(
            "file-content-v1",
            "回读时目标文件不存在；不重复写入",
        )),
    }
}

pub(crate) fn local_event(payload: &Value, ctx: &ExecContext) -> Result<EvidenceCheck> {
    let p: EventPayload = serde_json::from_value(payload.clone())?;
    let event = CalendarRepo::new(ctx.db)
        .get(&p.uid)?
        .ok_or_else(|| Error::NotFound("回读时日历事件不存在".into()))?;
    let expected = json!([p.uid, p.title, p.starts_at, p.ends_at, p.location]);
    let observed = json!([
        event.uid,
        event.title,
        event.starts_at,
        event.ends_at,
        event.location
    ]);
    Ok(EvidenceCheck::compare(
        "local-event-v1",
        "calendar_db",
        &p.uid,
        expected.to_string().as_bytes(),
        observed.to_string().as_bytes(),
    ))
}

pub(crate) fn mail(
    sender: &dyn MailSender,
    from: &str,
    payload: &Value,
    receipt: &str,
    _ctx: &ExecContext,
) -> Result<EvidenceCheck> {
    let p: EmailPayload = serde_json::from_value(payload.clone())?;
    let id = receipt
        .strip_prefix("sent ")
        .and_then(|s| s.split(" to ").next())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Invalid("发送回执缺少消息标识".into()))?;
    let m = sender.read_sent(id)?;
    if m.body.ends_with("[……正文过长已截断]") {
        return Ok(EvidenceCheck::unknown(
            "sent-mail-v1",
            "邮件回读被截断，证据不足",
        ));
    }
    let expected = json!([
        from.to_ascii_lowercase(),
        addresses(p.to),
        p.subject,
        p.body.replace("\r\n", "\n").trim_end()
    ]);
    let observed = json!([
        m.from_addr.to_ascii_lowercase(),
        addresses(m.to),
        m.subject,
        m.body.replace("\r\n", "\n").trim_end()
    ]);
    Ok(EvidenceCheck::compare(
        "sent-mail-v1",
        "sent_message_read",
        id,
        expected.to_string().as_bytes(),
        observed.to_string().as_bytes(),
    ))
}

pub(crate) fn addresses(a: Vec<String>) -> Vec<String> {
    let mut a: Vec<_> = a
        .into_iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    a.sort();
    a.dedup();
    a
}
