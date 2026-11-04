// hatch web client: thin UI over the product API v1 (/v1/...).
// No build step, no framework. The bearer token lives only in
// localStorage and is sent as an Authorization header, never in a
// URL and never logged.

const $ = (sel) => document.querySelector(sel);
const TOKEN_KEY = "hatch_api_token";
let currentSession = null;
let streaming = false;

function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function notice(text, isError) {
  const el = $("#notice");
  if (!text) { el.hidden = true; el.textContent = ""; return; }
  el.hidden = false;
  el.textContent = text;
  el.classList.toggle("error", !!isError);
}

function headers(json) {
  const h = {};
  if (json) h["Content-Type"] = "application/json";
  const t = localStorage.getItem(TOKEN_KEY);
  if (t) h["Authorization"] = "Bearer " + t;
  return h;
}

// One JSON API call. Throws an Error with the server's message.
async function api(path, opts) {
  const o = opts || {};
  const res = await fetch(path, { method: o.method || "GET",
    headers: headers(!!o.body), body: o.body ? JSON.stringify(o.body) : undefined });
  const text = await res.text();
  let data = null;
  try { data = JSON.parse(text); } catch (e) { /* plain-text body */ }
  if (!res.ok) {
    if (res.status === 401) throw new Error("未鉴权：请先在上方粘贴 daemon 的 bearer token 并保存。");
    const msg = data && data.error ? data.error.message : text;
    throw new Error(res.status + " " + (msg || "请求失败"));
  }
  return data;
}

function fmtTime(s) {
  if (!s) return "—";
  const d = new Date(s);
  return isNaN(d) ? String(s) : d.toLocaleString();
}


function tokenState() {
  const saved = !!localStorage.getItem(TOKEN_KEY);
  $("#token-state").textContent = saved ? "已保存 token（只存在本浏览器）。" : "未保存 token：API 调用会被拒绝。";
  $("#token").value = saved ? "••••••••" : "";
}

$("#save-token").addEventListener("click", () => {
  const v = $("#token").value.trim();
  if (!v || v === "••••••••") { notice("先粘贴 token 再保存。", true); return; }
  localStorage.setItem(TOKEN_KEY, v);
  tokenState();
  notice("token 已保存到本浏览器。");
  refreshAll();
});
$("#clear-token").addEventListener("click", () => {
  localStorage.removeItem(TOKEN_KEY);
  tokenState();
  notice("token 已清除。");
});

async function health() {
  const pill = $("#conn-status");
  try {
    await api("/v1/health");
    pill.textContent = "daemon 已连接";
    pill.classList.add("ok");
    pill.classList.remove("bad");
  } catch (e) {
    pill.textContent = "daemon 未连接";
    pill.classList.add("bad");
    pill.classList.remove("ok");
  }
}

// ---- tabs ----

document.querySelectorAll("#tabs button").forEach((b) => {
  b.addEventListener("click", () => {
    document.querySelectorAll("#tabs button").forEach((x) => x.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
    b.classList.add("active");
    $("#tab-" + b.dataset.tab).classList.add("active");
    if (b.dataset.tab === "approvals") loadApprovals();
    if (b.dataset.tab === "jobs") loadJobs();
  });
});

function showTab(name) {
  document.querySelector(`#tabs button[data-tab="${name}"]`).click();
}

// ---- sessions ----

async function loadSessions() {
  try {
    const data = await api("/v1/sessions?limit=20");
    const ul = $("#session-list");
    ul.innerHTML = "";
    for (const s of data.sessions || []) {
      const li = document.createElement("li");
      li.dataset.id = s.id;
      if (s.id === currentSession) li.classList.add("active");
      li.innerHTML = esc(s.title) + `<small>${esc(fmtTime(s.created_at))} · ${s.messages} 条</small>`;
      li.addEventListener("click", () => selectSession(s.id));
      ul.appendChild(li);
    }
    if (!data.sessions || data.sessions.length === 0) {
      ul.innerHTML = "<li>暂无会话，点上方新建。</li>";
    }
  } catch (e) { notice("加载会话失败：" + e.message, true); }
}

function renderMessage(m) {
  const msg = m.message || {};
  const role = msg.role || "unknown";
  const div = document.createElement("div");
  if (role === "tool") {
    div.className = "msg tool";
    div.innerHTML = `<span class="meta">工具结果 ${esc(msg.tool_call_id || "")}</span>${esc(msg.content || "").slice(0, 500)}`;
    return div;
  }
  div.className = "msg " + (role === "user" ? "user" : "assistant");
  let html = `<span class="meta">${esc(role)}</span>${esc(msg.content || "")}`;
  for (const c of msg.tool_calls || []) {
    html += `<span class="tool-call">调用 ${esc(c.name)} ${esc(JSON.stringify(c.arguments || {}))}</span>`;
  }
  div.innerHTML = html;
  return div;
}

async function selectSession(id) {
  currentSession = id;
  document.querySelectorAll("#session-list li").forEach((li) => {
    li.classList.toggle("active", li.dataset.id === id);
  });
  try {
    const data = await api("/v1/sessions/" + encodeURIComponent(id));
    const box = $("#messages");
    box.innerHTML = "";
    if (data.summary) {
      const s = document.createElement("div");
      s.className = "msg tool";
      s.innerHTML = "<span class=\"meta\">摘要</span>" + esc(data.summary);
      box.appendChild(s);
    }
    for (const m of data.messages || []) box.appendChild(renderMessage(m));
    if (!data.messages || data.messages.length === 0) {
      box.innerHTML = "<p class=\"hint\">空会话，发第一条消息吧。</p>";
    }
    box.scrollTop = box.scrollHeight;
  } catch (e) { notice("打开会话失败：" + e.message, true); }
}

$("#new-session").addEventListener("click", async () => {
  try {
    const data = await api("/v1/sessions", { method: "POST", body: { title: "网页会话" } });
    currentSession = data.id;
    await loadSessions();
    await selectSession(data.id);
  } catch (e) { notice("新建会话失败：" + e.message, true); }
});

// ---- streaming turn ----

// Parse one SSE chunk buffer into complete events.
function parseSse(buffer) {
  const events = [];
  let rest = buffer;
  for (;;) {
    const idx = rest.indexOf("\n\n");
    if (idx === -1) break;
    const raw = rest.slice(0, idx);
    rest = rest.slice(idx + 2);
    let name = "message";
    let data = "";
    for (const line of raw.split("\n")) {
      if (line.startsWith(":")) continue;
      if (line.startsWith("event:")) name = line.slice(6).trim();
      if (line.startsWith("data:")) data += line.slice(5).trim();
    }
    if (data) events.push({ name, data });
  }
  return { events, rest };
}

$("#send-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  if (streaming) return;
  const input = $("#send-text");
  const text = input.value.trim();
  if (!text || !currentSession) {
    notice(currentSession ? "先输入消息。" : "先选一个会话。", true);
    return;
  }
  input.value = "";
  const box = $("#messages");
  const userDiv = document.createElement("div");
  userDiv.className = "msg user";
  userDiv.innerHTML = "<span class=\"meta\">user</span>" + esc(text);
  box.appendChild(userDiv);
  const asstDiv = document.createElement("div");
  asstDiv.className = "msg assistant";
  asstDiv.innerHTML = "<span class=\"meta\">assistant</span>";
  box.appendChild(asstDiv);
  const status = $("#turn-status");
  status.textContent = "助手正在回答…";
  streaming = true;
  $("#send-btn").disabled = true;
  let answer = "";
  try {
    const res = await fetch(
      "/v1/sessions/" + encodeURIComponent(currentSession) + "/messages",
      {
        method: "POST",
        headers: { ...headers(true), Accept: "text/event-stream" },
        body: JSON.stringify({ text }),
      },
    );
    if (!res.ok) {
      const t = await res.text();
      let msg = t;
      try { msg = JSON.parse(t).error.message; } catch (e) { /* keep text */ }
      throw new Error(res.status + " " + msg);
    }
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const parsed = parseSse(buffer);
      buffer = parsed.rest;
      for (const e of parsed.events) {
        let d = {};
        try { d = JSON.parse(e.data); } catch (err) { /* ignore */ }
        if (e.name === "message" && d.delta) {
          answer += d.delta;
          asstDiv.innerHTML = "<span class=\"meta\">assistant</span>" + esc(answer);
        } else if (e.name === "tool_call" || e.name === "tool_result") {
          const line = document.createElement("div");
          line.className = "msg tool";
          line.textContent = (e.name === "tool_call" ? "调用：" : "结果：") + (d.detail || "");
          box.appendChild(line);
        } else if (e.name === "approval_needed") {
          status.textContent = "本回合需要审批："
            + (d.approval_ids || []).join(", ") + "（见“审批”页）";
          loadApprovals();
        } else if (e.name === "done") {
          status.textContent = "完成（" + d.stop + "，" + d.steps + " 步，"
            + (d.usage ? d.usage.prompt_tokens + " 提示 token" : "") + "）";
        } else if (e.name === "error") {
          throw new Error(d.error ? d.error.message : "回合失败");
        }
      }
      box.scrollTop = box.scrollHeight;
    }
    if (!answer) asstDiv.innerHTML = "<span class=\"meta\">assistant</span>（无文本回答）";
  } catch (e) {
    notice("发送失败：" + e.message, true);
    status.textContent = "";
  } finally {
    streaming = false;
    $("#send-btn").disabled = false;
    loadSessions();
  }
});

// ---- approvals ----

async function loadApprovals() {
  try {
    const data = await api("/v1/approvals");
    const list = $("#approvals-list");
    const items = data.approvals || [];
    const badge = $("#approvals-badge");
    badge.hidden = items.length === 0;
    badge.textContent = items.length;
    list.innerHTML = "";
    if (items.length === 0) {
      list.innerHTML = "<p class=\"hint\">暂无待审批事项。</p>";
      return;
    }
    for (const a of items) {
      const card = document.createElement("div");
      card.className = "card";
      card.innerHTML = `<h3>${esc(a.title)}</h3>
        <p class="preview">${esc(a.preview || "")}</p>
        <p class="meta">${esc(a.kind)} · ${esc(a.origin)} · 创建于 ${esc(fmtTime(a.created_at))}</p>
        <div class="actions">
          <button type="button" data-act="approve">批准并执行</button>
          <button type="button" class="danger" data-act="deny">拒绝</button>
        </div>`;
      card.querySelector('[data-act="approve"]').addEventListener("click", async () => {
        try {
          const r = await api("/v1/approvals/" + encodeURIComponent(a.id) + "/approve", { method: "POST", body: {} });
          notice("已批准，执行结果：" + r.outcome);
          loadApprovals();
        } catch (e) { notice("批准失败：" + e.message, true); }
      });
      card.querySelector('[data-act="deny"]').addEventListener("click", async () => {
        try {
          await api("/v1/approvals/" + encodeURIComponent(a.id) + "/deny", { method: "POST", body: {} });
          notice("已拒绝。");
          loadApprovals();
        } catch (e) { notice("拒绝失败：" + e.message, true); }
      });
      list.appendChild(card);
    }
  } catch (e) { notice("加载审批失败：" + e.message, true); }
}

$("#refresh-approvals").addEventListener("click", loadApprovals);

// ---- memory ----

$("#memory-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const q = $("#memory-q").value.trim();
  if (!q) return;
  const box = $("#memory-results");
  try {
    const data = await api("/v1/memory/search?q=" + encodeURIComponent(q) + "&k=10");
    box.innerHTML = "";
    const results = data.results || [];
    if (results.length === 0) {
      box.innerHTML = "<p class=\"hint\">没有找到相关记忆。</p>";
      return;
    }
    for (const c of results) {
      const card = document.createElement("div");
      card.className = "card";
      card.innerHTML = `<h3>${esc(c.subject)}</h3>
        <p>${esc(c.text)}</p>
        <p class="meta">来源 ${esc(c.source)} · 置信度 ${Number(c.confidence).toFixed(2)} · 评分 ${Number(c.score).toFixed(2)}${c.inferred ? " · 推断" : ""}</p>`;
      box.appendChild(card);
    }
  } catch (e) { notice("检索失败：" + e.message, true); }
});

// ---- jobs ----

async function loadJobs() {
  try {
    const data = await api("/v1/jobs");
    const box = $("#jobs-list");
    const jobs = data.jobs || [];
    if (jobs.length === 0) {
      box.innerHTML = "<p class=\"hint\">暂无任务。</p>";
      return;
    }
    let html = "<table class=\"jobs\"><tr><th>任务</th><th>类型</th><th>计划</th><th>启用</th><th>下次</th><th>上次状态</th><th></th></tr>";
    for (const j of jobs) {
      html += `<tr><td>${esc(j.name)}</td><td>${esc(j.kind)}</td><td>${esc(j.schedule)}</td>`
        + `<td>${j.enabled ? "是" : "否"}</td><td>${esc(fmtTime(j.next_run_at))}</td>`
        + `<td>${esc(j.last_status || "—")}</td>`
        + `<td><button type="button" data-run="${esc(j.id)}">立即执行</button></td></tr>`;
    }
    box.innerHTML = html + "</table>";
    box.querySelectorAll("[data-run]").forEach((b) => {
      b.addEventListener("click", async () => {
        try {
          const r = await api("/v1/jobs/" + encodeURIComponent(b.dataset.run) + "/run", { method: "POST", body: {} });
          notice("已执行：" + (r.summary || r.status || "完成"));
          loadJobs();
        } catch (e) { notice("执行失败：" + e.message, true); }
      });
    });
  } catch (e) { notice("加载任务失败：" + e.message, true); }
}

$("#refresh-jobs").addEventListener("click", loadJobs);

// ---- init ----

function refreshAll() {
  health();
  loadSessions();
  loadApprovals();
  loadJobs();
}

tokenState();
refreshAll();
