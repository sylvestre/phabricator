// Checks the scripts of the Rust revision page in a real browser, on the
// development stack (docker-compose.rust.yml, seeded by seed-dev-stack.sh):
// headless Chromium, driven through the DevTools protocol, writes an inline
// comment, reloads, tries the typeahead and the preview, then submits, which
// PUBLISHES the comment on the revision. Needs node 22+ and chromium.
//
//   node rust/scripts/browser-check.mjs <phsid of a seeded user> [D1]
//
// Until Playwright flows exist (TESTING.md, layer 9), this is the check of
// phab.js.
import { spawn } from "node:child_process";

const token = process.argv[2];
const revision = process.argv[3] ?? "D1";
const chrome = spawn("chromium", [
  "--headless=new", "--remote-debugging-port=9333", "--proxy-server=http://localhost:1090",
  "--no-first-run", "--user-data-dir=/tmp/e2e-chrome", "about:blank",
], { stdio: "ignore" });

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let targets;
for (let i = 0; i < 50; i++) {
  try {
    targets = await (await fetch("http://127.0.0.1:9333/json")).json();
    if (targets.some((t) => t.type === "page")) break;
  } catch {}
  await sleep(200);
}
const page = targets.find((t) => t.type === "page");
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve) => ws.addEventListener("open", resolve));
let id = 0;
const pending = new Map();
const events = [];
ws.addEventListener("message", (message) => {
  const data = JSON.parse(message.data);
  if (data.id && pending.has(data.id)) {
    pending.get(data.id)(data);
    pending.delete(data.id);
  } else if (data.method) {
    events.push(data);
  }
});
const send = (method, params = {}) => new Promise((resolve) => {
  const call = ++id;
  pending.set(call, resolve);
  ws.send(JSON.stringify({ id: call, method, params }));
});
const evaluate = async (expression) => {
  const result = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (result.result?.exceptionDetails) throw new Error(JSON.stringify(result.result.exceptionDetails));
  return result.result?.result?.value;
};
const waitFor = async (expression, what) => {
  for (let i = 0; i < 50; i++) {
    if (await evaluate(expression)) return;
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}`);
};

try {
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Network.enable");
  for (const [name, value] of [["phsid", token], ["phab_ui", "rust"]]) {
    await send("Network.setCookie", { name, value, domain: "phabricator.test", path: "/" });
  }
  await send("Page.navigate", { url: `http://phabricator.test/${revision}` });
  await waitFor(`document.readyState === "complete" && !!document.querySelector("td.n[data-line]")`, "the diff");
  const errors = events.filter((e) => e.method === "Runtime.exceptionThrown");
  console.log("script errors on load:", errors.length);

  // Click the first new-side line number, write, keep.
  const line = await evaluate(`(() => {
    const cell = document.querySelector('td.n[data-side="new"][data-line]');
    cell.click();
    return cell.dataset.line;
  })()`);
  await waitFor(`!!document.querySelector(".inline-editor textarea")`, "the editor");
  const stamp = `Inline from the e2e script ${Date.now()}`;
  await evaluate(`(() => {
    const textarea = document.querySelector(".inline-editor textarea");
    textarea.value = ${JSON.stringify(stamp)};
    document.querySelector(".inline-keep").click();
  })()`);
  console.log("draft shown:", await evaluate(`document.querySelector(".inline-draft-text")?.textContent`));
  console.log("count:", await evaluate(`document.querySelector(".inline-count").textContent`));
  console.log("field:", await evaluate(`document.querySelector('input[name="inlines"]').value`));

  // Reload: the draft is kept.
  await send("Page.reload");
  await sleep(500);
  await waitFor(`document.readyState === "complete" && !!document.querySelector("td.n[data-line]")`, "the reload");
  console.log("draft after reload:", await evaluate(`document.querySelectorAll(".inline-draft").length`));

  // Typeahead and preview while we are here.
  await evaluate(`(() => {
    const input = document.querySelector('input[name="reviewers"]');
    input.value = "bo";
    input.dispatchEvent(new Event("input"));
    const textarea = document.querySelector('textarea[name="comment"]');
    textarea.value = "Published with **one** inline.";
    textarea.dispatchEvent(new Event("input"));
  })()`);
  await waitFor(`document.querySelector("datalist")?.options.length > 0`, "suggestions");
  console.log("suggestions:", await evaluate(`[...document.querySelector("datalist").options].map((o) => o.value + " / " + o.label).join(", ")`));
  await waitFor(`!document.getElementById("comment-preview").hidden`, "the preview");
  console.log("preview:", await evaluate(`document.getElementById("comment-preview").innerHTML`));
  await evaluate(`document.querySelector('input[name="reviewers"]').value = ""`);

  // Submit: the inline is published, the draft forgotten.
  await evaluate(`document.getElementById("revision-actions").submit()`);
  await sleep(1500);
  await waitFor(`document.readyState === "complete" && !!document.querySelector("td.n[data-line]")`, "the published page");
  console.log("url after submit:", await evaluate(`location.href`));
  console.log("drafts left:", await evaluate(`document.querySelectorAll(".inline-draft").length`), await evaluate(`localStorage.length`));
  console.log("published inline:", await evaluate(`[...document.querySelectorAll(".inline-comment[data-phid] .remarkup")].some((n) => n.textContent.includes(${JSON.stringify(stamp)}))`));
  console.log("reply buttons:", await evaluate(`document.querySelectorAll("button.inline-reply").length`));
  console.log("published on line:", line);
  console.log("script errors:", events.filter((e) => e.method === "Runtime.exceptionThrown").map((e) => e.params.exceptionDetails.exception?.description).join(" | ") || "none");
} finally {
  ws.close();
  chrome.kill();
}
