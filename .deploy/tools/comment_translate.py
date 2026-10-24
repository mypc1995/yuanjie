#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""批量把源码注释翻成中文的工具链。

    extract.py <repo> <out_dir> <batch_lines> <batch_index>
        从仓库里挑出含英文注释的文件, 按批输出 JSON 任务单。

    apply.py <repo> <out_dir> <batch_index> <translations.json>
        把翻译回填进源码, 并验证「非注释部分逐字节未变」。

设计要点:
  * 注释行与代码行一一对应 (1:1), 回填时行号不会漂移。
  * 只翻译注释正文, 标识符 / 路径 / URL / 反引号里的代码原样保留。
  * 回填后必须通过「去掉注释后的代码骨架完全一致」校验, 否则回滚该文件。
"""
import json
import os
import re
import sys

SKIP_DIRS = {"lang", "target", ".git", "build", ".dart_tool", "ephemeral"}
EXTS = (".rs", ".dart")

# 注释标记: 行首 (缩进 + 标记 + 可选空白)
LINE_RE = re.compile(r"^(\s*(?:///|//!|//|/\*|\*)\s?)(.*)$")
EN_RE = re.compile(r"[A-Za-z]{3,}")


def is_translatable(text: str) -> bool:
    """是否是需要翻译的英文正文。"""
    if not text.strip():
        return False
    # 纯符号 / 纯数字 / 单个词 (如 TODO / FIXME) 不动
    if not EN_RE.search(text):
        return False
    if re.fullmatch(r"\s*[-=*_/\\|<>~^*+#]+\s*", text):
        return False
    if re.fullmatch(r"\s*(TODO|FIXME|XXX|NOTE|HACK|BUG|WARNING|ERROR)\b.*", text):
        return False
    return True


def iter_source_files(repo: str):
    for root, dirs, files in os.walk(repo):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in sorted(files):
            if name.endswith(EXTS):
                yield os.path.join(root, name)


def extract_file(repo: str, path: str):
    """返回该文件里需要翻译的注释行列表。"""
    rel = os.path.relpath(path, repo).replace("\\", "/")
    with open(path, "r", encoding="utf-8", errors="replace", newline="") as f:
        text = f.read()
    lines = text.split("\n")
    out = []
    in_block = False
    for i, line in enumerate(lines):
        stripped = line.strip()
        if in_block:
            if stripped.endswith("*/"):
                in_block = False
            m = LINE_RE.match(line)
            if m:
                prefix, body = m.group(1), m.group(2)
                body = body[:-2] if body.endswith("*/") else body
                if is_translatable(body):
                    out.append({"id": len(out) + 1, "line": i + 1,
                                "prefix": prefix, "text": body})
            continue
        if stripped.startswith("/*") and not stripped.endswith("*/"):
            in_block = True
        m = LINE_RE.match(line)
        if not m:
            continue
        prefix, body = m.group(1), m.group(2)
        suffix = ""
        if prefix.strip() == "/*":
            body = body[2:].strip() if body.startswith("*") else body.strip()
            if body.endswith("*/"):
                # 单行块注释: /* xxx */ —— 结尾的 */ 要留着, 回填时补回去
                cut = body.rindex("*/")
                head = body[:cut]
                text = head.rstrip()
                suffix = body[len(text):]
                body = text
        if is_translatable(body):
            out.append({"id": len(out) + 1, "line": i + 1,
                        "prefix": prefix, "suffix": suffix, "text": body})
    return {"file": rel, "comments": out}


def _is_char_literal(text: str, i: int) -> bool:
    """Rust 的字符字面量: 'a' 或 '\\n' 这种。返回 False 表示这是别的引号用法。"""
    if i + 2 >= len(text):
        return False
    if text[i + 1] == "\\":
        j = i + 2
        while j < len(text) and text[j] != "'":
            j += 1
        return j < len(text) and j - i <= 10
    return text[i + 2] == "'"


def _raw_string_end(text: str, i: int):
    """判断 i 处的引号是不是「原始字符串」的开头, 返回 (是?, 结束符)。"""
    # Rust: r"..." / r#"..."#    Dart: r'...'
    j = i - 1
    hashes = 0
    while j >= 0 and text[j] == "#":
        hashes += 1
        j -= 1
    if j >= 0 and text[j] == "r" and (j == 0 or not (text[j - 1].isalnum() or text[j - 1] in "_$")):
        return True, '"' + "#" * hashes
    return False, None


def code_skeleton(text: str, lang: str = "rs") -> str:
    """去掉注释和空白, 只留代码 —— 用来验证翻译没碰到代码。

    要区分: Rust 字符字面量 ('a') / Rust 原始字符串 (r#"..."#) /
    Dart 单引号字符串 ('abc') / Dart 原始字符串 (r'...') / 三引号字符串。
    分错了就会把字符串里的 // 当成注释, 代码骨架算错。
    """
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        raw = False
        end = None
        if c == '"' or (c == "'" and lang != "rs") or \
           (c == "'" and lang == "rs" and _is_char_literal(text, i)):
            triple = text[i:i + 3] in ('"""', "'''")
            if triple:
                end = c * 3
            else:
                is_raw, raw_end = _raw_string_end(text, i)
                if is_raw:
                    raw = True
                    end = raw_end
                else:
                    end = c
        if end is not None:
            out.append(end)
            i += len(end)
            while i < n:
                if not raw and text[i] == "\\":
                    out.append(text[i:i + 2])
                    i += 2
                    continue
                if text[i:i + len(end)] == end:
                    out.append(end)
                    i += len(end)
                    break
                out.append(text[i])
                i += 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] != "\n":
                i += 1
            continue
        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 1
            i += 2
            while i < n and depth:
                if text[i:i + 2] == "/*":
                    depth += 1
                    i += 2
                elif text[i:i + 2] == "*/":
                    depth -= 1
                    i += 2
                else:
                    i += 1
            out.append(" ")
            continue
        out.append(c)
        i += 1
    return re.sub(r"\s+", "", "".join(out))


def cmd_extract(repo, out_dir, batch_lines, batch_index):
    """把文件按注释行数分批, 输出第 batch_index 批的任务单。"""
    batches = []
    cur = []
    cur_lines = 0
    for path in iter_source_files(repo):
        info = extract_file(repo, path)
        if not info["comments"]:
            continue
        n = len(info["comments"])
        if cur and cur_lines + n > batch_lines:
            batches.append(cur)
            cur, cur_lines = [], 0
        cur.append(info)
        cur_lines += n
    if cur:
        batches.append(cur)
    if batch_index >= len(batches):
        print(json.dumps({"batch": batch_index, "files": [], "total": 0},
                         ensure_ascii=False, indent=2))
        return
    batch = batches[batch_index]
    payload = {
        "batch": batch_index,
        "batch_total": len(batches),
        "repo": repo,
        "files": batch,
        "total": sum(len(f["comments"]) for f in batch),
    }
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, "batch-%03d.json" % batch_index)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=1)
    os.replace(tmp, path)   # 原子替换, 避免并发的 worker 读到写了一半的文件
    print("批 %d/%d: %d 个文件, %d 行注释 -> %s" %
          (batch_index + 1, len(batches), len(batch), payload["total"], path))


def cmd_apply(repo, out_dir, batch_index, trans_path):
    """把某一批的翻译回填进源码。"""
    with open(os.path.join(out_dir, "batch-%03d.json" % batch_index),
              encoding="utf-8") as f:
        payload = json.load(f)
    with open(trans_path, encoding="utf-8") as f:
        trans = json.load(f)

    ok_files = 0
    fail_files = []
    for finfo in payload["files"]:
        rel = finfo["file"]
        path = os.path.join(repo, rel.replace("/", os.sep))
        tmap = trans.get(rel)
        if not tmap:
            fail_files.append((rel, "没有翻译"))
            continue
        with open(path, "r", encoding="utf-8", errors="replace", newline="") as f:
            original = f.read()
        lines = original.split("\n")
        for c in finfo["comments"]:
            new = tmap.get(str(c["id"]))
            if new is None:
                continue
            idx = c["line"] - 1
            prefix = c["prefix"]
            suffix = c.get("suffix", "")
            if not suffix:
                # 老任务单没有 suffix 字段, 从原行兜底推断单行块注释的结尾
                orig_line = lines[idx] if idx < len(lines) else ""
                stripped = orig_line.strip()
                if stripped.startswith("/*") and stripped.endswith("*/"):
                    cut = orig_line.rindex("*/")
                    suffix = orig_line[cut:]
                    if prefix.strip() == "/*":
                        suffix = " " + suffix
            sep = "" if prefix.endswith(" ") else " "
            lines[idx] = prefix + sep + new + suffix
        updated = "\n".join(lines)
        lang = "dart" if rel.endswith(".dart") else "rs"
        if code_skeleton(original, lang) != code_skeleton(updated, lang):
            fail_files.append((rel, "代码骨架被改动, 已回滚"))
            continue
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(updated)
        ok_files += 1

    print("批 %d: 回填成功 %d 个文件, 失败 %d 个" % (batch_index, ok_files, len(fail_files)))
    for rel, why in fail_files:
        print("  失败 %s: %s" % (rel, why))
    return 1 if fail_files else 0


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    cmd = sys.argv[1]
    if cmd == "extract":
        cmd_extract(sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]))
        return 0
    if cmd == "apply":
        return cmd_apply(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5])
    if cmd == "count":
        repo = sys.argv[2]
        n_files = 0
        n_lines = 0
        for path in iter_source_files(repo):
            info = extract_file(repo, path)
            if info["comments"]:
                n_files += 1
                n_lines += len(info["comments"])
        print("需翻译: %d 个文件, %d 行注释" % (n_files, n_lines))
        return 0
    print("未知命令", cmd)
    return 2


if __name__ == "__main__":
    sys.exit(main())
