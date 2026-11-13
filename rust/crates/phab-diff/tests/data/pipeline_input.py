"""Writes pipeline_input.json, the hunks pipeline.php runs through PHP's
DifferentialHunkParser to produce pipeline_expected.json."""

import json
import os


def h(old_offset, old_len, new_offset, new_len, changes):
    return {
        "old_offset": old_offset,
        "old_len": old_len,
        "new_offset": new_offset,
        "new_len": new_len,
        "changes": changes,
    }


def context_lines(prefix, count):
    return "".join(" %s%d\n" % (prefix, i) for i in range(1, count + 1))


CASES = [
    ("simple_change", [h(1, 3, 1, 3, " a\n-b\n+c\n d\n")], 3),
    ("add_only", [h(0, 0, 1, 2, "+x\n+y\n")], 3),
    ("delete_only", [h(1, 2, 0, 0, "-x\n-y\n")], 3),
    ("no_newline_old", [h(1, 1, 1, 1, "-x\n\\ No newline at end of file\n+x\n")], 3),
    ("no_newline_new", [h(1, 1, 1, 1, "-x\n+x\n\\ No newline at end of file\n")], 3),
    ("indent_spaces", [h(1, 2, 1, 2, "-foo();\n-  bar();\n+  foo();\n+bar();\n")], 3),
    ("indent_tabs", [h(1, 1, 1, 1, "-\tx = 1;\n+\t\tx = 1;\n")], 3),
    ("indent_mixed", [h(1, 1, 1, 1, "-  \tx\n+ \t \tx\n")], 3),
    ("indent_and_change", [h(1, 1, 1, 1, "-  return a;\n+    return b;\n")], 3),
    ("same_depth_different_ws", [h(1, 1, 1, 1, "-\tx\n+  x\n")], 3),
    (
        "unicode",
        [h(1, 1, 1, 1, "-Grumpy Cat ☃ says no\n+Grumpy Cät ☃ says yes\n")],
        3,
    ),
    ("combining_leading", [h(1, 1, 1, 1, "-́abc\n+́abd\n")], 3),
    ("long_line", [h(1, 1, 1, 1, "-" + "a" * 900 + "\n+" + "a" * 899 + "b\n")], 3),
    ("long_glyphs", [h(1, 1, 1, 1, "-" + "ab" * 60 + "\n+" + "ac" * 60 + "\n")], 3),
    ("crlf", [h(1, 2, 1, 2, " a\r\n-b\r\n+c\r\n")], 3),
    ("stripped_space", [h(1, 3, 1, 3, " a\n\n-b\n+c\n")], 3),
    ("empty_changes", [h(1, 0, 1, 0, "")], 3),
    (
        "multiple_hunks",
        [h(3, 3, 3, 3, " c\n-d\n+D\n e\n"), h(20, 3, 20, 4, " t\n+u\n u2\n v\n")],
        1,
    ),
    (
        "context_mask",
        [
            h(
                1,
                20,
                1,
                20,
                context_lines("l", 9) + "-x\n+y\n" + context_lines("m", 9) + "-p\n+q\n",
            )
        ],
        2,
    ),
    ("context_zero", [h(1, 5, 1, 5, " a\n b\n-c\n+C\n d\n e\n")], 0),
    (
        "smoothing",
        [h(1, 1, 1, 1, "-private function a($a, $b) {\n+public function and($b, $c) {\n")],
        3,
    ),
    ("replace_block", [h(1, 3, 1, 2, "-one\n-two\n-three\n+uno\n+dos\n")], 3),
    ("unbalanced", [h(1, 2, 1, 4, "-a\n+b\n+c\n+d\n e\n")], 3),
    # Realignment: whole files, where whitespace changes are aligned.
    (
        "realign_reindent",
        [
            h(
                1,
                5,
                1,
                6,
                "-if (x) {\n-  a();\n-  b();\n-}\n+if (x)\n+{\n+    a();\n+    b(1);\n+}\n c\n",
            )
        ],
        3,
    ),
    (
        "realign_whitespace_only",
        [h(1, 3, 1, 3, "-a = 1;\n-b=2;\n-c  =  3;\n+a=1;\n+b = 2;\n+c = 3;\n")],
        3,
    ),
    (
        "realign_moved_line",
        [h(1, 4, 1, 4, " one\n-two\n three\n-four\n+four  \n+  two\n")],
        3,
    ),
    (
        "realign_tabs",
        [h(1, 3, 1, 3, " {\n-\treturn x;\n+    return x;\n }\n")],
        3,
    ),
    (
        "realign_no_newline",
        [h(1, 2, 1, 2, " a\n-b\n\\ No newline at end of file\n+ b\n")],
        3,
    ),
    (
        "realign_skipped_offset",
        [h(2, 2, 2, 2, "-x = 1;\n+x=1;\n y\n")],
        3,
    ),
]

if __name__ == "__main__":
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pipeline_input.json")
    with open(path, "w", encoding="utf-8") as output:
        json.dump(
            [{"name": n, "hunks": hs, "context": c} for n, hs, c in CASES],
            output,
            indent=1,
            ensure_ascii=False,
        )
        output.write("\n")
