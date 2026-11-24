#!/usr/bin/env python3
"""Compares what PHP's and Rust's versions of the same pages show, on the
development stack.

    rust/scripts/page-compare.py <phsid> [urls.txt]

Each page is fetched twice through the stack's proxy: with the `phab_ui=rust`
cookie (Rust, when it serves the route) and with `X-Phab-Skip-Rust` (PHP).
The two pages don't share markup, so the comparison is of the words a reader
sees: how many of the words of PHP's page are on Rust's page, and the reverse,
ignoring the chrome both draw around the content: words found on most of
PHP's pages (menus, the search form's filters, footers). A page Rust doesn't
serve (no "old interface" link) is reported as PHP's.
"""

import html
import re
import subprocess
import sys

PROXY = "http://localhost:1090"
STACK = "http://phabricator.test"

PAGES = [
    "/", "/D1", "/D2", "/D3", "/D50", "/D7", "/differential/",
    "/differential/diff/1/", "/p/alice/", "/p/bob/", "/people/",
    "/project/", "/project/view/1/", "/project/members/1/", "/tag/build_reviewers/",
    "/feed/", "/notification/", "/H3", "/H9", "/herald/", "/herald/transcript/",
    "/diffusion/", "/diffusion/2/", "/diffusion/2/browse/master/",
    "/diffusion/2/history/master/", "/diffusion/2/branches/", "/diffusion/2/tags/",
    "/harbormaster/", "/harbormaster/build/1/", "/harbormaster/plan/1/",
    "/B1", "/harbormaster/lint/1/", "/harbormaster/unit/1/", "/harbormaster/unit/view/1/",
    "/settings/user/alice/", "/settings/user/alice/page/datetime/",
    "/dashboard/", "/applications/", "/file/", "/flag/", "/conduit/",
]

# Words of the page chrome (menus, footers) that differ by design.
CHROME = re.compile(r"\b(old interface|new interface|phabricator|log out|"
                    r"search|settings|notifications|help)\b", re.I)


def fetch(path, phsid, rust):
    headers = ["-b", f"phsid={phsid}" + ("; phab_ui=rust" if rust else "")]
    if not rust:
        headers += ["-H", "X-Phab-Skip-Rust: 1"]
    done = subprocess.run(
        ["curl", "-s", "-L", "-x", PROXY, "-w", "\n%{http_code}"] + headers
        + [STACK + path], capture_output=True, text=True, errors="replace")
    body, _, status = done.stdout.rpartition("\n")
    return status, body


def words(page):
    page = re.sub(r"(?is)<(script|style|head)\b.*?</\1>", " ", page)
    text = html.unescape(re.sub(r"(?s)<[^>]+>", " ", page))
    text = CHROME.sub(" ", text)
    return set(re.findall(r"[\w#@.-]{3,}", text.lower()))


def main():
    phsid = sys.argv[1]
    pages = PAGES
    if len(sys.argv) > 2:
        pages = [line.strip() for line in open(sys.argv[2]) if line.strip()]
    fetched = []
    for path in pages:
        php_status, php = fetch(path, phsid, rust=False)
        rust_status, rust = fetch(path, phsid, rust=True)
        fetched.append((path, php_status, words(php), rust_status, words(rust),
                        "old interface" in rust))
    # The chrome: words on more than half of PHP's pages.
    counts = {}
    for _, _, a, _, _, _ in fetched:
        for word in a:
            counts[word] = counts.get(word, 0) + 1
    chrome = {word for word, count in counts.items() if count > len(fetched) / 2}
    print(f"{'page':40} {'php':>4} {'rust':>4}  php-in-rust  rust-in-php")
    for path, php_status, a, rust_status, b, served in fetched:
        if not served:
            print(f"{path:40} {php_status:>4} {rust_status:>4}  (PHP serves it)")
            continue
        a, b = a - chrome, b - chrome
        both = a & b
        cover = len(both) / len(a) if a else 1.0
        back = len(both) / len(b) if b else 1.0
        flag = "" if cover >= 0.85 and php_status == rust_status else "  <-- check"
        print(f"{path:40} {php_status:>4} {rust_status:>4}  {cover:10.0%}  "
              f"{back:10.0%}{flag}")
        if flag:
            print(f"    only on PHP's page: {' '.join(sorted(a - b)[:20])}")
            print(f"    only on Rust's page: {' '.join(sorted(b - a)[:12])}")


if __name__ == "__main__":
    main()
