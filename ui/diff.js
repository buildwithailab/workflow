"use strict";

function reviewPatch(patch) {
  const lines = [];
  const notes = [];
  let oldLine, newLine, remainingOld = 0, remainingNew = 0;
  for (const raw of patch.split("\n")) {
    const line = raw.replace(/\r$/, "");
    const hunk = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
    if (hunk) {
      if (lines.length) lines.push({ tag: "gap" });
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[3]);
      remainingOld = Number(hunk[2] ?? 1);
      remainingNew = Number(hunk[4] ?? 1);
    } else if (line.startsWith("\\ ") && lines.length) {
      lines.at(-1).noNewline = true;
    } else if ((remainingOld || remainingNew) && /^[ +\-]/.test(line)) {
      const tag = { " ": "equal", "+": "insert", "-": "delete" }[line[0]];
      lines.push({
        tag,
        old: tag === "insert" ? null : oldLine++,
        new: tag === "delete" ? null : newLine++,
        text: line.slice(1),
      });
      if (tag !== "insert") remainingOld--;
      if (tag !== "delete") remainingNew--;
    } else if (/^Binary files .* differ$|^GIT binary patch$/.test(line)) {
      return { binary: true };
    } else if (/^(old mode |new mode |similarity index |rename from |rename to |Submodule )/.test(line)) {
      notes.push(line);
    }
  }
  return { lines, notes };
}

function changedSpan(before, after) {
  const old = [...before], next = [...after];
  let start = 0, end = 0;
  while (start < Math.min(old.length, next.length) && old[start] === next[start]) start++;
  while (end < Math.min(old.length, next.length) - start &&
    old[old.length - 1 - end] === next[next.length - 1 - end]) end++;
  return [old, next].map((chars) => [
    chars.slice(0, start).join(""),
    chars.slice(start, chars.length - end).join(""),
    chars.slice(chars.length - end).join(""),
  ]);
}

function reviewStats(lines) {
  const stats = { added: 0, removed: 0 };
  for (const line of lines) {
    if (line.tag === "insert") stats.added++;
    if (line.tag === "delete") stats.removed++;
  }
  return stats;
}

function diffStatsLabel(stats) {
  const label = el("span", "change-stats");
  label.append(
    el("span", "added", "+" + stats.added),
    el("span", "removed", "−" + stats.removed),
  );
  return label;
}

function renderDiff(target, result, onSelect, compact = false) {
  target.replaceChildren();
  if (result.binary) {
    target.append(el("p", "empty", tr("İkili dosya değişikliği")));
    return;
  }
  const lines = result.lines ?? [];
  if (result.notes?.length)
    target.append(el("pre", "review-notes", result.notes.join("\n")));
  if (!lines.length) {
    if (!result.notes?.length)
      target.append(el("p", "empty", tr("Gösterilecek satır farkı yok.")));
    return;
  }

  const view = el("div", "review-diff" + (compact ? " compact" : "")),
    body = el("div", "review-lines"),
    chunks = [],
    controls = el("div", "review-controls"),
    position = el("small", "review-position");
  position.setAttribute("aria-live", "polite");
  let current = -1;
  const go = (offset) => {
    current = Math.max(0, Math.min(chunks.length - 1, current + offset));
    for (const chunk of chunks)
      chunk.classList.toggle("current", chunk === chunks[current]);
    const scroller = target;
    scroller.scrollTop += chunks[current].getBoundingClientRect().top -
      controls.getBoundingClientRect().bottom;
    position.textContent = (current + 1) + " / " + chunks.length;
    previous.disabled = current === 0;
    next.disabled = current === chunks.length - 1;
  };
  const previous = button("↑", () => go(-1), "quiet", tr("Önceki değişiklik")),
    next = button("↓", () => go(1), "quiet", tr("Sonraki değişiklik")),
    split = button(tr("Yan yana"), () => {
      const enabled = view.classList.toggle("split");
      split.setAttribute("aria-pressed", String(enabled));
    }, "quiet review-layout");
  previous.setAttribute("aria-label", previous.title);
  next.setAttribute("aria-label", next.title);
  previous.disabled = true;
  split.setAttribute("aria-pressed", "false");
  controls.append(previous, next, position);
  if (!compact) controls.append(split);

  const row = (line, highlight, className = "") => {
    const node = el("div", "diff-line review-line " + line.tag + " " + className);
    const picker = el("span", "review-picker");
    if (onSelect && line.tag !== "equal") {
      const input = el("input");
      input.type = "checkbox";
      input.dataset.index = line.index;
      input.setAttribute("aria-label", tr("Satırı seç") + " " + (line.new ?? line.old));
      input.onchange = onSelect;
      picker.append(input);
    }
    const code = el("span", "review-code");
    if (highlight) code.append(highlight[0], el("mark", "", highlight[1]), highlight[2]);
    else code.textContent = line.text.replace(/\r?\n$/, "");
    if (line.noNewline)
      code.append(el("small", "review-newline", tr("Satır sonu yok")));
    node.append(picker,
      el("span", "number old", line.old ?? ""),
      el("span", "number new", line.new ?? ""),
      el("span", "review-sign", { equal: "", delete: "−", insert: "+" }[line.tag]),
      code);
    return node;
  };
  const contextRow = (line) => {
    const pair = el("div", "review-context");
    pair.append(row(line, null, "context-old"), row(line, null, "context-new"));
    return pair;
  };

  for (let start = 0; start < lines.length;) {
    if (lines[start].tag === "gap") {
      body.append(el("div", "review-gap", "…"));
      start++;
      continue;
    }
    let end = start + 1;
    if (lines[start].tag === "equal") {
      while (lines[end]?.tag === "equal") end++;
      const first = end - start <= 8 ? end : start === 0 ? start : start + 3;
      const last = first === end ? end : end === lines.length ? end : end - 3;
      for (let i = start; i < first; i++) body.append(contextRow(lines[i]));
      if (last > first) {
        const folded = el("details", "review-fold");
        folded.append(el("summary", "", tr("{count} değişmeyen satır", { count: last - first })));
        folded.addEventListener("toggle", () => {
          if (!folded.open || folded.childElementCount > 1) return;
          const fragment = document.createDocumentFragment();
          for (let i = first; i < last; i++) fragment.append(contextRow(lines[i]));
          folded.append(fragment);
        });
        body.append(folded);
      }
      for (let i = last; i < end; i++) body.append(contextRow(lines[i]));
    } else {
      while (lines[end] && !["equal", "gap"].includes(lines[end].tag)) end++;
      const edits = lines.slice(start, end);
      const deleted = edits.filter((line) => line.tag === "delete");
      const inserted = edits.filter((line) => line.tag === "insert");
      const chunk = el("div", "review-chunk");
      const left = el("div", "review-deleted"), right = el("div", "review-inserted");
      for (let index = 0; index < Math.max(deleted.length, inserted.length); index++) {
        const before = deleted[index], after = inserted[index];
        const spans = before && after && before.text.length + after.text.length <= 8000
          ? changedSpan(before.text.replace(/\r?\n$/, ""), after.text.replace(/\r?\n$/, ""))
          : [];
        if (before) left.append(row(before, spans[0]));
        if (after) right.append(row(after, spans[1]));
      }
      chunk.append(left, right);
      body.append(chunk);
      chunks.push(chunk);
    }
    start = end;
  }
  next.disabled = !chunks.length;
  position.textContent = chunks.length ? "– / " + chunks.length : "";
  view.append(controls, body);
  target.append(view);
  target.scrollTop = 0;
  target.scrollLeft = 0;
}
