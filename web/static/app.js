/*
 * Refinery's local interface.
 *
 * Three rules hold throughout this file.
 *
 * 1. Nothing is ever rendered by assigning markup. Every dynamic node is
 *    cloned from a <template> in index.html and filled with textContent.
 *    Transcripts, question labels, refined prompts, and delivery errors are
 *    all text somebody else wrote — a model, a caller, an upstream service —
 *    and the one reliable way to keep that text from becoming markup is never
 *    to have a code path that could turn it into markup.
 *
 * 2. The token lives in sessionStorage and travels in the Authorization
 *    header. It arrives in the URL because `refinery open` has no other way to
 *    hand it to a browser, and it is stripped from the address bar on the
 *    first load so it does not linger in history or in a screenshot.
 *
 * 3. The page never decides anything the daemon decides. Which actions are
 *    available comes from the case's state, which comes from the state
 *    machine; the buttons are disabled to match, but the daemon refuses the
 *    same operations independently.
 */

"use strict";

const TOKEN_KEY = "refinery.token";

const state = {
  token: null,
  caseId: null,
  detail: null,
  stream: null,
  streamCase: null,
  pollTimer: null,
  refreshTimer: null,
  states: [],
};

/* Token ------------------------------------------------------------------- */

function readToken() {
  const url = new URL(window.location.href);
  const fromQuery = url.searchParams.get("token");
  if (fromQuery) {
    store(fromQuery);
    url.searchParams.delete("token");
    window.history.replaceState({}, "", url.pathname + url.search + url.hash);
    return fromQuery;
  }
  try {
    return window.sessionStorage.getItem(TOKEN_KEY);
  } catch (error) {
    return null;
  }
}

function store(token) {
  try {
    window.sessionStorage.setItem(TOKEN_KEY, token);
  } catch (error) {
    /* A browser with storage disabled still works for this one page load. */
  }
}

/* Transport --------------------------------------------------------------- */

async function api(path, options) {
  const settings = Object.assign({ headers: {} }, options || {});
  settings.headers = Object.assign(
    { Authorization: "Bearer " + state.token },
    settings.headers
  );
  if (settings.body !== undefined) {
    settings.headers["Content-Type"] = "application/json";
    settings.body = JSON.stringify(settings.body);
  }
  const response = await fetch(path, settings);
  if (response.status === 401) {
    state.token = null;
    render();
    throw new Error("This browser session is not authorized. Reopen with `refinery open`.");
  }
  if (response.status === 204) {
    return null;
  }
  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch (error) {
      body = null;
    }
  }
  if (!response.ok) {
    throw new Error(errorMessage(body, response.status));
  }
  return body;
}

function errorMessage(body, status) {
  if (body && typeof body.message === "string" && body.message) {
    return body.message;
  }
  return "The daemon returned HTTP " + status + ".";
}

/* Small DOM helpers ------------------------------------------------------- */

const $ = (id) => document.getElementById(id);

function clone(templateId) {
  return $(templateId).content.firstElementChild.cloneNode(true);
}

function fill(root, selector, text) {
  const node = root.querySelector(selector);
  if (node) {
    node.textContent = text === null || text === undefined ? "" : String(text);
  }
  return node;
}

function empty(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

function show(node, visible) {
  node.hidden = !visible;
}

function pairs(container, entries) {
  empty(container);
  for (const [term, value] of entries) {
    if (value === null || value === undefined || value === "") {
      continue;
    }
    const row = clone("tpl-pair");
    fill(row, "dt", term);
    fill(row, "dd", value);
    container.appendChild(row);
  }
}

function announce(message) {
  $("announcer").textContent = message;
}

function pageError(message) {
  const node = $("page-error");
  node.textContent = message || "";
  show(node, Boolean(message));
}

function moment(value) {
  if (!value) {
    return "";
  }
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? String(value) : parsed.toLocaleString();
}

function shortId(value) {
  return typeof value === "string" && value.length > 12 ? value.slice(0, 8) : value;
}

function bytes(count) {
  if (typeof count !== "number") {
    return "";
  }
  const units = ["B", "KB", "MB", "GB"];
  let size = count;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return (unit === 0 ? size : size.toFixed(1)) + " " + units[unit];
}

/* State presentation ------------------------------------------------------ */

const TONES = {
  completed: "ok",
  ready: "ok",
  accepted: "ok",
  awaiting_answer: "wait",
  delivering: "wait",
  in_flight: "wait",
  failed: "bad",
  cancelled: "bad",
};

function badge(node, value) {
  node.textContent = String(value || "").replace(/_/g, " ");
  const tone = TONES[value];
  if (tone) {
    node.dataset.tone = tone;
  } else {
    delete node.dataset.tone;
  }
}

/* Routing ----------------------------------------------------------------- */

const VIEWS = {
  locked: "view-locked",
  cases: "view-cases",
  case: "view-case",
  repositories: "view-repositories",
  settings: "view-settings",
  logs: "view-logs",
};

function currentRoute() {
  const hash = window.location.hash.replace(/^#\/?/, "");
  const parts = hash.split("/").filter(Boolean);
  if (parts.length === 0) {
    return { name: "cases" };
  }
  if (parts[0] === "cases" && parts[1]) {
    return { name: "case", id: parts[1] };
  }
  if (VIEWS[parts[0]] && parts[0] !== "case") {
    return { name: parts[0] };
  }
  return { name: "cases" };
}

function showView(name) {
  for (const [route, id] of Object.entries(VIEWS)) {
    show($(id), route === name);
  }
  for (const link of document.querySelectorAll(".nav a")) {
    const active = link.dataset.route === name || (name === "case" && link.dataset.route === "cases");
    if (active) {
      link.setAttribute("aria-current", "page");
    } else {
      link.removeAttribute("aria-current");
    }
  }
}

async function render() {
  pageError("");
  stopStream();
  stopPolling();

  if (!state.token) {
    showView("locked");
    return;
  }

  const route = currentRoute();
  showView(route.name);
  try {
    if (route.name === "cases") {
      await loadStates();
      await renderCases();
      startPolling(renderCases);
    } else if (route.name === "case") {
      state.caseId = route.id;
      await renderCase(route.id);
      startStream(route.id);
    } else if (route.name === "repositories") {
      await renderRepositories();
    } else if (route.name === "settings") {
      await renderSettings();
    } else if (route.name === "logs") {
      await renderLogs();
    }
  } catch (error) {
    pageError(error.message);
  }
}

/* Case list --------------------------------------------------------------- */

async function renderCases() {
  const filter = $("filter-state").value;
  const query = filter ? "?state=" + encodeURIComponent(filter) : "";
  const cases = await api("/v1/refinements" + query);
  const body = $("cases-body");
  empty(body);
  for (const item of cases) {
    const row = clone("tpl-case-row");
    const link = row.querySelector(".case-link");
    // The link's accessible name is "Case <id>": the word is static markup in
    // the template so the name never depends on this assignment landing.
    link.setAttribute("href", "#/cases/" + item.id);
    fill(row, ".case-id", shortId(item.id));
    badge(row.querySelector(".case-state"), item.state);
    fill(row, ".case-request", item.request_id);
    fill(row, ".case-updated", moment(item.updated_at));
    body.appendChild(row);
  }
  show($("cases-empty"), cases.length === 0);
  show($("cases-table"), cases.length > 0);
  connection("Connected");
}

/* Case detail ------------------------------------------------------------- */

async function renderCase(id) {
  const detail = await api("/v1/refinements/" + encodeURIComponent(id));
  state.detail = detail;

  $("case-heading").textContent = "Case " + shortId(detail.id);
  badge($("case-state"), detail.state);
  $("case-times").textContent =
    "created " + moment(detail.created_at) + " · updated " + moment(detail.updated_at);

  renderActions(detail);
  renderPending(detail);
  renderOutput(detail);
  renderInputs(detail);
  renderThreads(detail);
  renderDeliveries(detail);
  renderEvents(detail);
  connection("Connected");
}

const TERMINAL = ["completed", "failed", "cancelled"];

function renderActions(detail) {
  const copy = $("action-copy");
  const retry = $("action-retry");
  const cancel = $("action-cancel");
  copy.disabled = !detail.output;
  retry.disabled = detail.state !== "failed" || !detail.output;
  cancel.disabled = TERMINAL.includes(detail.state);
}

function renderPending(detail) {
  const section = $("pending-section");
  const container = $("answer-questions");
  empty(container);
  show($("answer-error"), false);
  const pending = detail.pending_question;
  show(section, Boolean(pending));
  if (!pending) {
    return;
  }
  $("pending-prompt").textContent = pending.prompt;
  pending.questions.forEach((question, index) => {
    container.appendChild(questionField(question, index));
  });
}

/*
 * Build one answer control for one question.
 *
 * Association is by nesting rather than by a for/id pair: a question request
 * can be rendered more than once in a session and generated ids would have to
 * be kept unique by hand. A control inside its own label needs no id at all
 * and cannot come apart.
 */
function questionField(question, index) {
  const required = Boolean(question.required);
  const suffix = required ? "" : " (optional)";

  if (question.response_type === "free_text") {
    const node = clone("tpl-question-text");
    fill(node, ".q-label", question.label + suffix);
    const help = fill(node, ".q-help", question.description || "");
    show(help, Boolean(question.description));
    const input = node.querySelector(".q-input");
    input.name = "q" + index;
    input.required = required;
    input.dataset.questionId = question.id;
    input.dataset.responseType = question.response_type;
    return node;
  }

  const multiple = question.response_type === "multiple_choice";
  const node = clone("tpl-question-choice");
  fill(node, ".q-label", question.label + suffix);
  const help = fill(node, ".q-help", question.description || "");
  show(help, Boolean(question.description));
  node.dataset.questionId = question.id;
  node.dataset.responseType = question.response_type;
  const choices = node.querySelector(".choices");
  for (const choice of question.choices || []) {
    const option = clone("tpl-choice");
    const input = option.querySelector(".choice-input");
    input.type = multiple ? "checkbox" : "radio";
    input.name = "q" + index;
    input.value = choice.value;
    fill(option, ".choice-label", choice.label || choice.value);
    choices.appendChild(option);
  }
  return node;
}

function collectAnswers(pending) {
  const answers = [];
  const now = new Date().toISOString();
  const container = $("answer-questions");

  for (const input of container.querySelectorAll(".q-input")) {
    const text = input.value.trim();
    if (text) {
      answers.push({
        question_id: input.dataset.questionId,
        value: { type: "text", text: text },
        answered_at: now,
        answered_by: "local-ui",
      });
    }
  }
  for (const group of container.querySelectorAll("fieldset.question")) {
    const selected = Array.from(group.querySelectorAll(".choice-input:checked")).map(
      (input) => input.value
    );
    if (selected.length === 0) {
      continue;
    }
    const multiple = group.dataset.responseType === "multiple_choice";
    answers.push({
      question_id: group.dataset.questionId,
      value: multiple
        ? { type: "choices", values: selected }
        : { type: "choice", value: selected[0] },
      answered_at: now,
      answered_by: "local-ui",
    });
  }

  return {
    schema_version: pending.schema_version,
    case_id: pending.case_id,
    question_request_id: pending.id,
    answers: answers,
  };
}

function renderOutput(detail) {
  const section = $("output-section");
  show(section, Boolean(detail.output));
  if (!detail.output) {
    $("output-text").value = "";
    return;
  }
  const prompt = detail.output.prompt;
  pairs($("output-summary"), [
    ["Title", prompt.title],
    ["Objective", prompt.objective],
    ["Accepted", moment(detail.output.accepted_at)],
  ]);
  $("output-text").value = promptText(prompt);
}

/*
 * The plain-text rendering of a refined prompt.
 *
 * This is what the copy action puts on the clipboard, so it has to be the
 * whole contract rather than the prompt field alone: a destination that
 * receives the envelope gets acceptance criteria and constraints, and a person
 * pasting into a chat window should get exactly the same brief.
 */
function promptText(prompt) {
  const lines = [prompt.title, "", prompt.prompt, ""];
  const sections = [
    ["Objective", prompt.objective ? [prompt.objective] : []],
    ["Context", prompt.context],
    ["Requirements", prompt.requirements],
    ["Constraints", prompt.constraints],
    ["Acceptance criteria", prompt.acceptance_criteria],
    ["Assumptions", prompt.assumptions],
    ["Unresolved questions", prompt.unresolved_questions],
  ];
  for (const [heading, items] of sections) {
    if (!items || items.length === 0) {
      continue;
    }
    lines.push(heading);
    for (const item of items) {
      lines.push("- " + item);
    }
    lines.push("");
  }
  if (prompt.references && prompt.references.length > 0) {
    lines.push("References");
    for (const reference of prompt.references) {
      lines.push("- " + referenceText(reference));
    }
    lines.push("");
  }
  return lines.join("\n").trimEnd() + "\n";
}

/*
 * A reference is one of three shapes: a repository path, an attachment, or an
 * external value such as a ticket. Each may carry a note saying why it
 * matters, and the note is worth keeping — it is often the only thing that
 * explains why a path is in the brief at all.
 */
function referenceText(reference) {
  const subject =
    reference.path || reference.value || reference.attachment_id || "unnamed reference";
  return reference.note ? subject + " — " + reference.note : subject;
}

function renderInputs(detail) {
  const request = detail.request;
  const destination = request.destination || {};
  pairs($("case-inputs"), [
    ["Case", detail.id],
    ["Request", detail.request_id],
    ["Source", (request.source && (request.source.system || request.source.kind)) || ""],
    ["Destination", destination.kind || destination.type || ""],
    ["Repository", request.repository || "none"],
  ]);

  const transcript = $("case-transcript");
  empty(transcript);
  const messages = (request.transcript && request.transcript.messages) || [];
  for (const message of messages) {
    const node = clone("tpl-transcript-message");
    fill(node, ".message-role", message.role);
    fill(node, ".message-text", message.text || message.content || "");
    transcript.appendChild(node);
  }

  const attachments = $("case-attachments");
  empty(attachments);
  for (const attachment of detail.attachments || []) {
    const node = clone("tpl-attachment");
    fill(node, ".attachment-name", attachment.name);
    fill(
      node,
      ".attachment-detail",
      [attachment.media_type, bytes(attachment.size_bytes), attachment.state]
        .filter(Boolean)
        .join(" · ")
    );
    attachments.appendChild(node);
  }
  show($("attachments-empty"), (detail.attachments || []).length === 0);
}

function renderThreads(detail) {
  const list = $("case-questions");
  empty(list);
  const threads = detail.questions || [];
  for (const thread of threads) {
    const node = clone("tpl-thread");
    fill(node, ".thread-prompt", thread.request.prompt);
    fill(
      node,
      ".thread-meta",
      thread.request.status + " · raised " + moment(thread.request.created_at)
    );
    const answers = node.querySelector(".thread-answers");
    for (const question of thread.request.questions) {
      const answer = thread.answers.find((item) => item.question_id === question.id);
      const row = clone("tpl-thread-answer");
      fill(row, ".thread-question", question.label);
      fill(row, ".thread-value", answer ? answerText(answer.value) : "— not answered");
      fill(row, ".thread-by", answer ? answer.answered_by : "");
      answers.appendChild(row);
    }
    list.appendChild(node);
  }
  show($("questions-empty"), threads.length === 0);
}

function answerText(value) {
  if (!value) {
    return "";
  }
  if (value.type === "text") {
    return value.text;
  }
  if (value.type === "choice") {
    return value.value;
  }
  if (value.type === "choices") {
    return (value.values || []).join(", ");
  }
  return "";
}

function renderDeliveries(detail) {
  const deliveries = detail.deliveries || [];
  const body = $("deliveries-body");
  empty(body);
  for (const delivery of deliveries) {
    const row = clone("tpl-delivery-row");
    fill(row, ".delivery-destination", delivery.destination_kind);
    badge(row.querySelector(".delivery-status"), delivery.status);
    fill(row, ".delivery-attempt", delivery.attempt);
    fill(
      row,
      ".delivery-detail",
      delivery.error || (delivery.status === "accepted" ? "accepted" : "")
    );
    fill(row, ".delivery-updated", moment(delivery.updated_at));
    body.appendChild(row);
  }
  show($("deliveries-empty"), deliveries.length === 0);
  show($("deliveries-table"), deliveries.length > 0);
}

function renderEvents(detail) {
  const list = $("case-events");
  empty(list);
  for (const event of detail.events || []) {
    list.appendChild(eventNode(event));
  }
  const shown = (detail.events || []).length;
  $("events-note").textContent =
    detail.event_count > shown
      ? "Showing the most recent " + shown + " of " + detail.event_count + " events."
      : shown + (shown === 1 ? " event." : " events.");
  list.scrollTop = list.scrollHeight;
}

function eventNode(event) {
  const node = clone("tpl-event");
  fill(node, ".event-sequence", event.sequence);
  fill(node, ".event-kind", String(event.type || "").replace(/_/g, " "));
  fill(node, ".event-detail", eventDetail(event));
  const time = node.querySelector(".event-time");
  time.textContent = moment(event.occurred_at);
  time.setAttribute("datetime", event.occurred_at || "");
  return node;
}

/*
 * A one-line summary of an event's payload.
 *
 * Events are a flattened tagged union, so rather than teach this page every
 * variant, it names the fields worth reading and falls back to listing the
 * remaining scalar fields. A payload variant added later still renders.
 */
function eventDetail(event) {
  const skip = new Set(["schema_version", "id", "case_id", "sequence", "occurred_at", "type"]);
  const parts = [];
  for (const [key, value] of Object.entries(event)) {
    if (skip.has(key) || value === null || value === undefined) {
      continue;
    }
    if (typeof value === "object") {
      continue;
    }
    parts.push(key.replace(/_/g, " ") + " " + value);
  }
  return parts.join(" · ");
}

/* Live updates ------------------------------------------------------------ */

function connection(message, status) {
  const node = $("connection");
  node.textContent = message;
  if (status) {
    node.dataset.status = status;
  } else {
    delete node.dataset.status;
  }
}

/*
 * EventSource cannot set an Authorization header, which is why the daemon also
 * accepts the token as a query parameter. The stream is only a change signal:
 * every notification re-fetches the case detail rather than trying to apply an
 * event to the rendered page, so a missed or duplicated event cannot leave the
 * view describing a case that never existed.
 */
function startStream(caseId) {
  const url =
    "/v1/refinements/" +
    encodeURIComponent(caseId) +
    "/events?follow=1&token=" +
    encodeURIComponent(state.token);
  const stream = new EventSource(url);
  state.stream = stream;
  state.streamCase = caseId;
  // A single step of the agent loop can append several events at once, and
  // each one is only a signal to re-read. Coalescing them keeps one burst from
  // becoming one request per event.
  stream.addEventListener("case_event", () => {
    if (state.streamCase !== caseId || state.refreshTimer) {
      return;
    }
    state.refreshTimer = window.setTimeout(() => {
      state.refreshTimer = null;
      if (state.streamCase === caseId) {
        renderCase(caseId).catch((error) => pageError(error.message));
      }
    }, 120);
  });
  stream.addEventListener("open", () => connection("Live"));
  stream.addEventListener("error", () => connection("Reconnecting…", "error"));
}

function stopStream() {
  if (state.refreshTimer) {
    window.clearTimeout(state.refreshTimer);
    state.refreshTimer = null;
  }
  if (state.stream) {
    state.stream.close();
    state.stream = null;
    state.streamCase = null;
  }
}

function startPolling(task) {
  state.pollTimer = window.setInterval(() => {
    task().catch((error) => pageError(error.message));
  }, 4000);
}

function stopPolling() {
  if (state.pollTimer) {
    window.clearInterval(state.pollTimer);
    state.pollTimer = null;
  }
}

/* Repositories ------------------------------------------------------------ */

async function renderRepositories() {
  const repositories = await api("/v1/repositories");
  const list = $("repositories-list");
  empty(list);
  for (const repository of repositories) {
    const node = clone("tpl-repository");
    fill(node, ".repository-root", repository.root);
    const policy = repository.policy || {};
    fill(
      node,
      ".repository-policy",
      "read-only · up to " +
        bytes(policy.max_file_bytes) +
        " per file · " +
        policy.max_results +
        " results per call · ignore files " +
        (policy.respect_ignore_files ? "honoured" : "ignored")
    );
    list.appendChild(node);
  }
  show($("repositories-empty"), repositories.length === 0);
}

/* Settings and health ----------------------------------------------------- */

async function renderSettings() {
  const [health, settings] = await Promise.all([api("/v1/health"), api("/v1/settings")]);

  pairs($("health-pairs"), [
    ["Status", health.status],
    ["Version", health.version],
    ["Contract version", health.schema_version],
    ["Data directory", health.data_dir],
    ["Listening on", health.listen],
    ["Provider", health.provider + " · " + health.model],
    ["Provider credential", health.provider_credential ? "stored" : "not configured"],
    ["Pending migrations", health.pending_migrations],
    ["Database integrity", (health.integrity_problems || []).join("; ") || "ok"],
  ]);

  const counts = Object.entries(health.cases_by_state || {});
  pairs($("health-counts"), counts.map(([key, value]) => [key.replace(/_/g, " "), value]));
  show($("counts-empty"), counts.length === 0);

  const jobs = Object.entries(health.jobs_by_status || {});
  pairs($("health-jobs"), jobs.map(([key, value]) => [key.replace(/_/g, " "), value]));
  show($("jobs-empty"), jobs.length === 0);

  pairs($("settings-pairs"), [
    ["API port", settings.api.port],
    ["Log filter", settings.logging.filter],
    ["Log format", settings.logging.json ? "JSON" : "human-readable"],
    ["Retained log files", settings.logging.retained_files],
    ["Backend", settings.provider.backend],
    ["Model", settings.provider.model],
    ["Max transcript bytes", bytes(settings.limits.max_transcript_bytes)],
    ["Max file bytes", bytes(settings.limits.max_file_bytes)],
    ["Max attachment bytes", bytes(settings.limits.max_attachment_bytes)],
    ["Max results per call", settings.limits.max_results],
    ["Overlord base URL", (settings.overlord && settings.overlord.base_url) || "not configured"],
  ]);
}

/* Logs -------------------------------------------------------------------- */

async function renderLogs() {
  const lines = Number($("logs-lines").value) || 200;
  const body = await api("/v1/logs?lines=" + encodeURIComponent(lines));
  const text = $("logs-text");
  text.value = (body.lines || []).join("\n");
  text.scrollTop = text.scrollHeight;
}

/* Actions ----------------------------------------------------------------- */

/*
 * Run one case action with the button showing that it is working.
 *
 * Availability is restored from the case's state rather than by re-enabling
 * the button, because the action usually changed that state: cancelling a case
 * makes "Cancel case" wrong, and a blanket re-enable would offer it again.
 */
async function guard(button, message, action) {
  const label = button.textContent;
  button.disabled = true;
  button.textContent = "Working…";
  pageError("");
  try {
    await action();
    announce(message);
  } catch (error) {
    pageError(error.message);
  } finally {
    button.textContent = label;
    if (state.detail) {
      renderActions(state.detail);
    } else {
      button.disabled = false;
    }
  }
}

function wire() {
  $("token-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const value = $("token-input").value.trim();
    if (!value) {
      return;
    }
    state.token = value;
    store(value);
    $("token-input").value = "";
    render();
  });

  $("case-filter").addEventListener("submit", (event) => {
    event.preventDefault();
    render();
  });

  $("logs-form").addEventListener("submit", (event) => {
    event.preventDefault();
    renderLogs().catch((error) => pageError(error.message));
  });

  $("answer-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const pending = state.detail && state.detail.pending_question;
    if (!pending) {
      return;
    }
    const error = $("answer-error");
    show(error, false);
    const payload = collectAnswers(pending);
    const button = event.target.querySelector("button[type=submit]");
    const label = button.textContent;
    button.disabled = true;
    button.textContent = "Submitting…";
    try {
      await api("/v1/refinements/" + encodeURIComponent(pending.case_id) + "/answers", {
        method: "POST",
        body: payload,
      });
      announce("Answers submitted. The case is resuming.");
      await renderCase(pending.case_id);
    } catch (failure) {
      error.textContent = failure.message;
      show(error, true);
    } finally {
      button.textContent = label;
      button.disabled = false;
    }
  });

  $("action-copy").addEventListener("click", async (event) => {
    const detail = state.detail;
    if (!detail || !detail.output) {
      return;
    }
    const text = promptText(detail.output.prompt);
    await guard(event.currentTarget, "Refined prompt copied to the clipboard.", async () => {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
        return;
      }
      // A page served over plain loopback HTTP may not have the async
      // clipboard, so fall back to selecting the text the user can copy.
      const area = $("output-text");
      area.focus();
      area.select();
      throw new Error(
        "This browser will not copy without a secure context. The prompt is selected — press the copy shortcut."
      );
    });
  });

  $("action-retry").addEventListener("click", (event) => {
    const id = state.caseId;
    guard(event.currentTarget, "Delivery retry queued.", async () => {
      await api("/v1/refinements/" + encodeURIComponent(id) + "/deliveries", { method: "POST" });
      await renderCase(id);
    });
  });

  $("action-cancel").addEventListener("click", (event) => {
    const id = state.caseId;
    guard(event.currentTarget, "Case cancelled.", async () => {
      await api("/v1/refinements/" + encodeURIComponent(id) + "/cancel", { method: "POST" });
      await renderCase(id);
    });
  });

  $("repository-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const error = $("repository-error");
    show(error, false);
    const body = { path: $("repo-path").value.trim() };
    const maxFile = Number($("repo-max-file").value);
    const maxResults = Number($("repo-max-results").value);
    if (maxFile > 0) {
      body.max_file_bytes = maxFile;
    }
    if (maxResults > 0) {
      body.max_results = maxResults;
    }
    body.respect_ignore_files = $("repo-ignore").checked;
    try {
      const registered = await api("/v1/repositories", { method: "POST", body: body });
      announce("Registered " + registered.root + ".");
      $("repo-path").value = "";
      await renderRepositories();
    } catch (failure) {
      error.textContent = failure.message;
      show(error, true);
    }
  });

  window.addEventListener("hashchange", () => {
    render();
    $("main").focus();
  });
}

/*
 * Fill the state filter from the daemon's own list of states.
 *
 * Asking rather than hard-coding means a state added to the machine appears
 * here without an edit, and a page served by an older daemon never offers a
 * filter that daemon would reject.
 */
async function loadStates() {
  if (state.states.length > 0) {
    return;
  }
  const health = await api("/v1/health");
  state.states = health.case_states || [];
  const select = $("filter-state");
  for (const value of state.states) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = value.replace(/_/g, " ");
    select.appendChild(option);
  }
}

async function boot() {
  state.token = readToken();
  wire();
  await render();
}

document.addEventListener("DOMContentLoaded", boot);
