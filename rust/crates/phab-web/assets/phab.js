// The few interactive parts of the Rust pages, without a framework.
"use strict";

// Comment previews: a textarea with data-preview="<id>" shows its text,
// rendered as Remarkup by the service, in the element with that id.
for (const textarea of document.querySelectorAll("textarea[data-preview]")) {
  const target = document.getElementById(textarea.dataset.preview);
  const token = document.querySelector('meta[name="csrf-token"]');
  if (!target || !token) {
    continue;
  }
  let timer = null;
  let latest = 0;
  textarea.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const request = ++latest;
      const text = textarea.value;
      if (text.trim() === "") {
        target.hidden = true;
        target.replaceChildren();
        return;
      }
      const body = new URLSearchParams({ __csrf__: token.content, text });
      try {
        const response = await fetch("/rust-ui/preview", { method: "POST", body });
        // An answer to an older request than the last one is stale.
        if (response.ok && request === latest) {
          // The service escapes the text: this is the same HTML as a comment.
          target.innerHTML = await response.text();
          target.hidden = false;
        }
      } catch (error) {
        // No preview: the comment can still be submitted.
      }
    }, 300);
  });
}

// Typeaheads: an input with data-typeahead suggests users and #projects
// for the name being typed, the last one of a comma or space separated
// list.
let typeaheads = 0;
for (const input of document.querySelectorAll("input[data-typeahead]")) {
  const list = document.createElement("datalist");
  list.id = `typeahead-${++typeaheads}`;
  input.after(list);
  input.setAttribute("list", list.id);
  let timer = null;
  let latest = 0;
  input.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(async () => {
      const request = ++latest;
      const match = input.value.match(/^(.*[,\s])?([^,\s]*)$/);
      const before = match[1] ?? "";
      const typed = match[2];
      if (typed.replace(/^[@#]/, "") === "") {
        list.replaceChildren();
        return;
      }
      try {
        const response = await fetch(`/rust-ui/typeahead?q=${encodeURIComponent(typed)}`);
        if (!response.ok || request !== latest) {
          return;
        }
        const options = (await response.json()).map((suggestion) => {
          const option = document.createElement("option");
          option.value = before + suggestion.value;
          option.label = suggestion.label;
          return option;
        });
        list.replaceChildren(...options);
      } catch (error) {
        // No suggestions: names can still be typed whole.
      }
    }, 150);
  });
}

// Inline comments: a click on a line number opens an editor under the
// line, and "Reply" one under a published comment. The comments are kept
// in the browser until the action form is sent, which publishes them in
// the same edit as the action and the comment (the `inline` transactions).
const diffSection = document.querySelector(".diff-section[data-revision]");
const actionForm = document.getElementById("revision-actions");
if (diffSection && actionForm) {
  const key = `phab-inlines:${diffSection.dataset.revision}`;
  const field = actionForm.elements.namedItem("inlines");
  const count = actionForm.querySelector(".inline-count");
  let drafts = [];
  try {
    drafts = JSON.parse(localStorage.getItem(key) ?? "[]");
  } catch (error) {
    drafts = [];
  }
  if (location.hash === "#inlines-published") {
    drafts = [];
    history.replaceState(null, "", location.pathname + location.search);
  }

  const save = () => {
    if (drafts.length > 0) {
      localStorage.setItem(key, JSON.stringify(drafts));
    } else {
      localStorage.removeItem(key);
    }
    field.value = drafts.length > 0 ? JSON.stringify(drafts) : "";
    count.hidden = drafts.length === 0;
    const plural = drafts.length === 1 ? "" : "s";
    count.textContent = `${drafts.length} inline comment${plural} will be published with this action.`;
  };

  const element = (name, className, text) => {
    const node = document.createElement(name);
    node.className = className;
    if (text !== undefined) {
      node.textContent = text;
    }
    return node;
  };
  const button = (label, className) => {
    const node = element("button", className, label);
    node.type = "button";
    return node;
  };
  const row = (content) => {
    const tr = element("tr", "diff-inline");
    const td = document.createElement("td");
    td.colSpan = 3;
    td.append(content);
    tr.append(td);
    return tr;
  };
  // Puts `tr` after `anchor` and the comments already under it.
  const placeAfter = (anchor, tr) => {
    let last = anchor;
    while (last.nextElementSibling?.classList.contains("diff-inline")) {
      last = last.nextElementSibling;
    }
    last.after(tr);
  };

  // The row a comment goes under: the comment it replies to, or the last
  // line it is on. None when the diff doesn't show it any more.
  const anchorOf = (draft) => {
    if (draft.replyToCommentPHID) {
      const phid = CSS.escape(draft.replyToCommentPHID);
      return diffSection.querySelector(`.inline-comment[data-phid="${phid}"]`)?.closest("tr");
    }
    const side = draft.isNewFile ? "new" : "old";
    const last = draft.line + (draft.length ?? 1) - 1;
    for (const changeset of diffSection.querySelectorAll(".changeset[data-path]")) {
      if (changeset.dataset.path === draft.path) {
        return changeset.querySelector(`td.n[data-side="${side}"][data-line="${last}"]`)?.closest("tr");
      }
    }
    return null;
  };

  let unplaced = null;
  let openEditor = null;
  const show = (draft) => {
    const box = element("div", "inline-comment inline-draft");
    const edit = button("Edit", "inline-edit");
    const remove = button("Remove", "inline-remove");
    box.append(element("p", "inline-author", "Not published yet"), element("div", "inline-draft-text", draft.content), edit, remove);
    const anchor = anchorOf(draft);
    let node = box;
    if (anchor) {
      node = row(box);
      placeAfter(anchor, node);
    } else {
      if (!unplaced) {
        unplaced = element("div", "inline-unplaced");
        unplaced.append(element("p", "muted", "Inline comments on lines this diff doesn't show:"));
        diffSection.querySelector("h2").after(unplaced);
      }
      box.prepend(element("p", "muted", `${draft.path ?? ""}:${draft.line ?? ""}`));
      unplaced.append(box);
    }
    remove.addEventListener("click", () => {
      drafts = drafts.filter((kept) => kept !== draft);
      node.remove();
      save();
    });
    edit.addEventListener("click", () => {
      // The editor replaces the comment until it is kept or cancelled.
      node.hidden = true;
      openEditor(node, draft, () => {
        node.hidden = false;
      }, () => {
        node.remove();
      });
    });
  };

  // An editor under `anchor` for a new comment at `place`, or for the
  // kept comment `place` itself when it has content.
  openEditor = (anchor, place, onCancel = () => {}, onKeep = () => {}) => {
    const textarea = document.createElement("textarea");
    textarea.rows = 4;
    textarea.value = place.content ?? "";
    textarea.setAttribute("aria-label", "Inline comment");
    const keep = button("Keep", "inline-keep");
    const cancel = button("Cancel", "inline-cancel");
    const box = element("div", "inline-comment inline-editor");
    box.append(textarea, keep, cancel);
    // Under a row of the diff, or in the list of unplaced comments; an
    // edited comment's editor takes its place.
    const node = anchor.tagName === "TR" ? row(box) : box;
    if (anchor.tagName === "TR" && place.content === undefined) {
      placeAfter(anchor, node);
    } else {
      anchor.after(node);
    }
    textarea.focus();
    cancel.addEventListener("click", () => {
      node.remove();
      onCancel();
    });
    keep.addEventListener("click", () => {
      if (textarea.value.trim() === "") {
        return;
      }
      const draft = { ...place, content: textarea.value };
      const index = drafts.indexOf(place);
      if (index >= 0) {
        drafts[index] = draft;
      } else {
        drafts.push(draft);
      }
      save();
      node.remove();
      onKeep();
      show(draft);
    });
  };

  for (const comment of diffSection.querySelectorAll(".inline-comment[data-phid]")) {
    comment.append(button("Reply", "inline-reply"));
  }
  diffSection.addEventListener("click", (event) => {
    const cell = event.target.closest("td.n[data-line]");
    if (cell) {
      const place = {
        path: cell.closest(".changeset[data-path]").dataset.path,
        line: Number(cell.dataset.line),
        isNewFile: cell.dataset.side === "new",
      };
      if (diffSection.dataset.diff) {
        place.diff = Number(diffSection.dataset.diff);
      }
      openEditor(cell.closest("tr"), place);
      return;
    }
    const reply = event.target.closest("button.inline-reply");
    if (reply) {
      const comment = reply.closest(".inline-comment[data-phid]");
      openEditor(comment.closest("tr"), { replyToCommentPHID: comment.dataset.phid });
    }
  });
  for (const draft of drafts) {
    show(draft);
  }
  save();
}

// Parts of Diffusion pages PHP computes, loaded from PHP as its own pages
// load them: the last change of each entry of a directory (a table with
// data-lastmodified), and the branches and tags of a commit (elements
// with data-php-fragment). PHP answers "for (;;);" and a JSON envelope.
async function phpPayload(uri, body) {
  const response = await fetch(uri, {
    method: body ? "POST" : "GET",
    body,
    credentials: "same-origin",
  });
  if (!response.ok) {
    return null;
  }
  const text = (await response.text()).replace(/^for \(;;\);/, "");
  const envelope = JSON.parse(text);
  return envelope.error ? null : envelope.payload;
}

const phpToken = document.querySelector('meta[name="csrf-token"]');
for (const table of document.querySelectorAll("table[data-lastmodified]")) {
  const rows = new Map();
  for (const row of table.querySelectorAll("tr[data-path]")) {
    rows.set(row.dataset.path, row);
  }
  if (rows.size === 0 || !phpToken) {
    continue;
  }
  const body = new URLSearchParams({
    __csrf__: phpToken.content,
    __ajax__: "1",
    __wflow__: "true",
    paths: JSON.stringify([...rows.keys()]),
  });
  phpPayload(table.dataset.lastmodified, body)
    .then((payload) => {
      for (const [path, columns] of Object.entries(payload || {})) {
        const row = rows.get(path);
        for (const cell of row ? row.querySelectorAll("[data-column]") : []) {
          // PHP escapes the columns: they are HTML of its own pages.
          if (columns[cell.dataset.column] !== undefined) {
            cell.innerHTML = columns[cell.dataset.column];
          }
        }
      }
    })
    .catch(() => {
      // The columns stay empty, as on PHP's page when the request fails.
    });
}
for (const element of document.querySelectorAll("[data-php-fragment]")) {
  const uri = new URL(element.dataset.phpFragment, window.location.href);
  uri.searchParams.set("__ajax__", "1");
  phpPayload(uri)
    .then((payload) => {
      if (typeof payload === "string") {
        element.innerHTML = payload;
      }
    })
    .catch(() => {});
}
