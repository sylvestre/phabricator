// Checks the scripts of the Rust revision page in a real browser, on the
// development stack (docker-compose.rust.yml, seeded by seed-dev-stack.sh):
// headless Firefox, driven through WebDriver BiDi (no geckodriver needed),
// writes an inline comment, reloads, tries the typeahead and the preview,
// then submits, which PUBLISHES the comment on the revision. Needs node 22+
// and Firefox (FIREFOX, firefox-nightly by default).
//
//   node rust/scripts/browser-check.mjs <phsid of a seeded user> [D1]
//
// Until Playwright flows exist (TESTING.md, layer 9), this is the check of
// phab.js.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const token = process.argv[2];
const revision = process.argv[3] ?? "D1";
const port = 9334;

// A fresh profile which reaches the stack through tinyproxy.
const profile = mkdtempSync(join(tmpdir(), "phab-firefox-"));
writeFileSync(join(profile, "user.js"), [
  'user_pref("network.proxy.type", 1);',
  'user_pref("network.proxy.http", "localhost");',
  'user_pref("network.proxy.http_port", 1090);',
  'user_pref("browser.shell.checkDefaultBrowser", false);',
  'user_pref("datareporting.policy.dataSubmissionEnabled", false);',
].join("\n"));
const firefox = spawn(process.env.FIREFOX ?? "firefox-nightly", [
  "--headless", "--no-remote", "--profile", profile, "--remote-debugging-port", String(port),
], { stdio: "ignore" });

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let ws;
for (let i = 0; i < 100 && !ws; i++) {
  await sleep(200);
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/session`);
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve);
      socket.addEventListener("error", reject);
    });
    ws = socket;
  } catch {
    // Not listening yet.
  }
}
if (!ws) {
  firefox.kill();
  throw new Error("Firefox did not start");
}

let id = 0;
const pending = new Map();
const errors = [];
ws.addEventListener("message", (message) => {
  const data = JSON.parse(message.data);
  if (data.id && pending.has(data.id)) {
    pending.get(data.id)(data);
    pending.delete(data.id);
  } else if (data.method === "log.entryAdded" && data.params.level === "error") {
    errors.push(data.params.text);
  }
});
const send = async (method, params = {}) => {
  const call = ++id;
  const answer = await new Promise((resolve) => {
    pending.set(call, resolve);
    ws.send(JSON.stringify({ id: call, method, params }));
  });
  if (answer.type === "error") {
    throw new Error(`${method}: ${answer.error} ${answer.message}`);
  }
  return answer.result;
};

// Evaluates `expression` in the page; its value must be a primitive.
let context;
const evaluate = async (expression) => {
  const result = await send("script.evaluate", {
    expression, target: { context }, awaitPromise: true, resultOwnership: "none",
  });
  if (result.type === "exception") {
    throw new Error(result.exceptionDetails.text);
  }
  return result.result.value;
};
const waitFor = async (expression, what) => {
  for (let i = 0; i < 50; i++) {
    if (await evaluate(`!!(${expression})`)) {
      return;
    }
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}`);
};

try {
  await send("session.new", { capabilities: {} });
  await send("session.subscribe", { events: ["log.entryAdded"] });
  context = (await send("browsingContext.getTree", {})).contexts[0].context;
  for (const [name, value] of [["phsid", token], ["phab_ui", "rust"]]) {
    await send("storage.setCookie", {
      cookie: { name, value: { type: "string", value }, domain: "phabricator.test", path: "/" },
    });
  }
  await send("browsingContext.navigate", {
    context, url: `http://phabricator.test/${revision}`, wait: "complete",
  });
  await waitFor(`document.querySelector("td.n[data-line]")`, "the diff");
  console.log("script errors on load:", errors.length);

  // Click the first new-side line number, write, keep.
  const line = await evaluate(`(() => {
    const cell = document.querySelector('td.n[data-side="new"][data-line]');
    cell.click();
    return cell.dataset.line;
  })()`);
  await waitFor(`document.querySelector(".inline-editor textarea")`, "the editor");
  const stamp = `Inline from the browser check ${Date.now()}`;
  const published = `${stamp} (edited)`;
  await evaluate(`(() => {
    document.querySelector(".inline-editor textarea").value = ${JSON.stringify(stamp)};
    document.querySelector(".inline-keep").click();
  })()`);
  console.log("draft shown:", await evaluate(`document.querySelector(".inline-draft-text")?.textContent`));
  console.log("count:", await evaluate(`document.querySelector(".inline-count").textContent`));
  console.log("field:", await evaluate(`document.querySelector('input[name="inlines"]').value`));

  // Edit it.
  await evaluate(`document.querySelector(".inline-edit").click()`);
  await waitFor(`document.querySelector(".inline-editor textarea")?.value === ${JSON.stringify(stamp)}`, "the edit");
  await evaluate(`(() => {
    document.querySelector(".inline-editor textarea").value += " (edited)";
    document.querySelector(".inline-keep").click();
  })()`);
  console.log("drafts after edit:", await evaluate(`document.querySelectorAll(".inline-draft").length`),
    await evaluate(`JSON.parse(document.querySelector('input[name="inlines"]').value).map((d) => d.content).join(" | ")`));

  // Reload: the draft is kept.
  await send("browsingContext.reload", { context, wait: "complete" });
  await waitFor(`document.querySelector("td.n[data-line]")`, "the reload");
  console.log("drafts after reload:", await evaluate(`document.querySelectorAll(".inline-draft").length`));

  // The typeahead and the preview.
  await evaluate(`(() => {
    const input = document.querySelector('input[name="reviewers"]');
    input.value = "bo";
    input.dispatchEvent(new Event("input"));
    const textarea = document.querySelector('textarea[name="comment"]');
    textarea.value = "Published with **one** inline.";
    textarea.dispatchEvent(new Event("input"));
  })()`);
  await waitFor(`document.querySelector("datalist")?.options.length > 0`, "suggestions");
  console.log("suggestions:", await evaluate(
    `[...document.querySelector("datalist").options].map((o) => o.value + " / " + o.label).join(", ")`));
  await waitFor(`!document.getElementById("comment-preview").hidden`, "the preview");
  console.log("preview:", await evaluate(`document.getElementById("comment-preview").innerHTML`));
  await evaluate(`document.querySelector('input[name="reviewers"]').value = ""`);

  // Submit: the inline is published, the draft forgotten.
  await evaluate(`document.getElementById("revision-actions").submit()`);
  await sleep(1500);
  await waitFor(`document.readyState === "complete" && document.querySelector("td.n[data-line]")`,
    "the published page");
  console.log("url after submit:", await evaluate(`location.href`));
  console.log("drafts left:", await evaluate(`document.querySelectorAll(".inline-draft").length`),
    await evaluate(`localStorage.length`));
  console.log("published:", await evaluate(
    `[...document.querySelectorAll(".inline-comment[data-phid] .remarkup")].some((n) => n.textContent.includes(${JSON.stringify(published)}))`));
  console.log("reply buttons:", await evaluate(`document.querySelectorAll("button.inline-reply").length`));
  console.log("on line:", line);
  console.log("script errors:", errors.join(" | ") || "none");
} finally {
  await send("session.end").catch(() => {});
  ws.close();
  firefox.kill();
  rmSync(profile, { recursive: true, force: true });
}
