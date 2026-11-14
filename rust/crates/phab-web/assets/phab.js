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
