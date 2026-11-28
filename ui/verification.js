"use strict";

// One state per session shared by both views. Merely opening, rendering or
// switching this panel only reads metadata; it never starts a CLI or sends input.
const verificationViews = new Map();
let verificationShown = null;
let verificationTimer;

function releaseVerificationReports(except = null) {
  for (const [id,view] of verificationViews) {
    if(id === except) continue;
    view.state = null;
    view.scopeCheckedAt = 0;
    view.readRevision = (view.readRevision || 0) + 1;
    view.readError = null;
  }
}

function resetVerificationViews() {
  clearTimeout(verificationTimer);
  verificationShown = null;
  verificationViews.clear();
  $("#verification-content").replaceChildren();
  delete $("#verification-content").dataset.editRevision;
  $("#verification-actions").replaceChildren();
  $("#verification-panel").hidden = true;
  $("#session-verification-view").setAttribute("aria-expanded","false");
  $("#chat-view").classList.toggle("has-verification",false);
}

function verificationView(id) {
  if (!verificationViews.has(id)) verificationViews.set(id, { open: false, state: null, busy: false });
  return verificationViews.get(id);
}

function verificationNode(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function renderVerificationView() {
  const selected = selectedChatSession();
  const id = selected?.id || null;
  const view = id && verificationView(id);
  const opened = !!view?.open;
  const switched = verificationShown !== (opened ? id : null);
  verificationShown = opened ? id : null;
  releaseVerificationReports(verificationShown);
  const panel = $("#verification-panel");
  $("#session-verification-view").setAttribute("aria-expanded", String(opened));
  panel.hidden = !opened;
  if (shownChatId && selected) {
    $("#chat-view").insertBefore(panel, $("#chat-composer"));
  } else {
    $(".main").insertBefore(panel, $(".main > .footer"));
  }
  $("#chat-view").classList.toggle("has-verification", opened && !!shownChatId);
  clearTimeout(verificationTimer);
  if (!opened) {
    $("#verification-content").replaceChildren();
    delete $("#verification-content").dataset.editRevision;
    $("#verification-actions").replaceChildren();
    return;
  }
  paintVerification(id, view);
  if (switched) readVerification(id, true).catch(error);
  else scheduleVerificationRead(id);
}

function scheduleVerificationRead(id) {
  if (verificationShown !== id) return;
  clearTimeout(verificationTimer);
  verificationTimer = setTimeout(() => readVerification(id, Date.now() - (verificationView(id).scopeCheckedAt || 0) >= 30000).catch(error), 1200);
}

async function readVerification(id, refresh) {
  const view = verificationView(id);
  if (view.reading || view.busy) { scheduleVerificationRead(id); return; }
  const revision = view.readRevision || 0;
  const current = () => verificationViews.get(id) === view;
  view.reading = true;
  if (refresh) { view.checking = true; paintVerification(id, view); }
  try {
    const state = await api("verification", "state", { id, refresh });
    if (!current() || verificationShown !== id || revision !== (view.readRevision || 0)) return;
    const changed = JSON.stringify(state) !== JSON.stringify(view.state);
    view.state = state;
    if (refresh) view.scopeCheckedAt = Date.now();
    view.readError = null;
    if (changed && view.editing && state.revision !== view.editRevision) view.editStale = true;
  } catch (err) {
    if (!current() || verificationShown !== id || revision !== (view.readRevision || 0)) return;
    view.readError = tr("Doğrulama durumu okunamadı; sonuçlar güncel kabul edilmez.");
    if (refresh) throw err;
  } finally {
    if(current()) {
      view.reading = false;
      view.checking = false;
      if (verificationShown === id) paintVerification(id, view);
      scheduleVerificationRead(id);
    }
  }
}

async function verificationAction(id, action, extra = {}) {
  const view = verificationView(id);
  if (view.busy || view.reading) return;
  const current = () => verificationViews.get(id) === view;
  view.busy = true;
  paintVerification(id, view);
  try {
    const state = await api("verification", action, { id, revision: view.state?.revision || 0, ...extra });
    if(!current()) return;
    view.state = verificationShown === id ? state : null;
    view.editing = null;
    view.editStale = false;
    view.readError = null;
  } catch (err) {
    if(current()) error(err);
  } finally {
    if(current()) {
      view.busy = false;
      if (verificationShown === id) {
        paintVerification(id, view);
        await readVerification(id, false);
      }
    }
  }
}

function paintVerification(id, view) {
  if (verificationShown !== id) return;
  const content = $("#verification-content");
  const actions = $("#verification-actions");
  const state = view.state || { phase: "idle" };
  const phase = state.phase;
  const pending = phase === "preparing" || phase === "running";
  const labels = {
    idle: "Hazırlanmadı", preparing: "Adımlar hazırlanıyor…", ready: "Başlatmanı bekliyor",
    running: "Doğrulanıyor…", reported: "Model raporu", stale: "Yeniden hazırlanmalı",
    interrupted: "İstek kesildi", error: "Doğrulama tamamlanamadı",
  };
  $("#verification-phase").textContent = tr(labels[phase] || labels.idle);
  $("#verification-refresh").disabled = !!(view.busy || view.reading || view.editing);
  actions.replaceChildren();
  const addAction = (title, handler, primary = false, disabled = false) => {
    const button = verificationNode("button", tr(title), primary ? "primary" : "quiet");
    button.type = "button";
    button.disabled = disabled || view.busy || view.checking || !!view.readError;
    button.onclick = handler;
    actions.append(button);
    return button;
  };
  if (view.editing) {
    // Do not replace focused fields on every state poll or workspace rerender.
    if (content.dataset.editRevision !== id + ":" + view.editRevision) paintVerificationEditor(id, view);
    if (view.editStale) {
      addAction("Liste değişti; düzenlemeyi iptal et", () => { view.editing = null; paintVerification(id, view); });
      return;
    }
    addAction("Adım ekle", () => {
      if (view.editing.length >= 20) return;
      view.editing.push({ title: "", check: "" });
      paintVerificationEditor(id, view);
      content.lastElementChild?.querySelector("input")?.focus();
    }, false, view.editing.length >= 20);
    addAction("Vazgeç", () => { view.editing = null; paintVerification(id, view); });
    addAction("Listeyi kaydet", () => {
      if (!view.editing.length || view.editing.some(item => !item.title.trim() || !item.check.trim())) {
        error(tr("Her adım için başlık ve kontrol açıklaması gerekli.")); return;
      }
      verificationAction(id, "edit", { items: view.editing });
    }, true);
    return;
  }
  delete content.dataset.editRevision;
  content.replaceChildren();
  let description = {
    idle: "Hazır olduğunda adımları bu oturumdaki modele hazırlat. Henüz hiçbir kontrol çalıştırılmaz.",
    preparing: "Model yalnızca adımları öneriyor. Kontroller, listeyi onaylayıp başlattığında çalışır.",
    ready: "Adımları düzenleyebilirsin. Hazır olduğunda doğrulamayı ayrıca başlat.",
    running: "Onayladığın liste bu oturumda uygulanıyor. Sorulara Sohbet veya Terminal üzerinden yanıt verebilirsin.",
    reported: "Aşağıdaki sonuçlar modelin bildirimidir; bağımsız ölçüm veya başarı garantisi değildir.",
    stale: "Kaynak, konuşma veya geliştirme isteği değişti. Önceki liste ve sonuçlar güncel değil.",
    interrupted: "Terminal veya istek değişti. Kayıtlar korundu; devam etmek için yeniden hazırla.",
    error: "İstek ya da kapsam kontrolü tamamlanamadı. Yeniden denemeden önce Terminaldeki taslağı kontrol et.",
  }[phase];
  content.append(verificationNode("p", view.readError || tr(description || ""), "help verification-note"));
  if (pending && state.delivery === "unconfirmed") content.append(verificationNode("p",
    tr("Mesajın teslim teyidi bekleniyor. Terminalde taslak varsa kontrol edip Enter ile gönderebilirsin. Aynı doğrulama isteği açık kalır."),
    "help verification-note"));
  const reason = {
    newMessage: "Neden: yeni bir geliştirme mesajı gönderildi.",
    sourceChanged: "Neden: kaynak dosyalar değişti.",
    bindingChanged: "Neden: terminal veya konuşma değişti.",
    stopped: "Neden: terminal durduruldu.",
    restarted: "Neden: uygulama yeniden açıldı.",
    expired: "Neden: isteğin süresi doldu.",
    cancelled: "Neden: istek iptal edildi.",
    scopeUnavailable: "Neden: kaynak kapsamı okunamadı.",
    deliveryFailed: "Neden: mesajın gönderimi doğrulanamadı.",
  }[state.reason];
  if (reason) content.append(verificationNode("p", tr(reason), "help verification-note"));
  if (view.checking) content.append(verificationNode("p", tr("Kaynak kapsamı kontrol ediliyor…"), "help"));
  if (state.items?.length) {
    const list = verificationNode("ol", undefined, "verification-items");
    for (const item of state.items) {
      const row = verificationNode("li");
      const report = state.results?.find(result => result.id === item.id);
      row.append(verificationNode("strong", item.title));
      row.append(verificationNode("p", item.check));
      if (report) {
        const status = { passed: "Başarılı bildirildi", failed: "Başarısız bildirildi", blocked: "Doğrulanamadı" }[report.status];
        row.append(verificationNode("span", tr(status || "Doğrulanamadı"), "verification-result " + report.status));
        row.append(verificationNode("p", report.evidence, "verification-evidence"));
      }
      list.append(row);
    }
    content.append(list);
  }
  if (!pending && state.items?.length) addAction("Listeyi düzenle", () => {
    view.editing = state.items.map(item => ({ title: item.title, check: item.check }));
    view.editRevision = state.revision;
    view.editStale = false;
    paintVerification(id, view);
  });
  if (pending) {
    addAction("İsteği iptal et", () => verificationAction(id, "cancel"));
    actions.append(verificationNode("small", tr("İptal, sonuç kabulünü durdurur; modelin çalışmasını durdurmak için Terminali kullan."), "help"));
  } else if (phase === "ready") {
    addAction("Doğrulamayı başlat", () => verificationAction(id, "start"), true, !active(id));
  } else {
    addAction("Doğrulamayı hazırla", () => verificationAction(id, "prepare"), true, !active(id));
  }
  if (!active(id)) actions.append(verificationNode("small", tr("Önce bu oturumun terminalini başlat."), "help"));
  const scope = verificationNode("small", tr("Kapsam: takip edilen kaynaklar ve hariç tutulmamış dosyalar. Yoksayılan bağımlılıklar kapsam dışıdır."), "help verification-scope");
  content.append(scope);
}

function paintVerificationEditor(id, view) {
  const content = $("#verification-content");
  content.replaceChildren();
  content.dataset.editRevision = id + ":" + view.editRevision;
  view.editing.forEach((item, index) => {
    const row = verificationNode("div", undefined, "verification-edit-item");
    const title = verificationNode("input");
    title.value = item.title;
    title.maxLength = 180;
    title.setAttribute("aria-label", tr("Adım başlığı") + " " + (index + 1));
    title.placeholder = tr("Adım başlığı");
    title.oninput = () => { item.title = title.value; };
    const check = verificationNode("textarea");
    check.value = item.check;
    check.maxLength = 2000;
    check.rows = 2;
    check.placeholder = tr("Nasıl kontrol edilecek, beklenen sonuç ne?");
    check.setAttribute("aria-label", tr("Kontrol açıklaması") + " " + (index + 1));
    check.oninput = () => { item.check = check.value; };
    const remove = verificationNode("button", "×", "icon quiet");
    remove.type = "button";
    remove.title = tr("Adımı kaldır");
    remove.setAttribute("aria-label", tr("Adımı kaldır") + " " + (index + 1));
    remove.onclick = () => { view.editing.splice(index, 1); paintVerificationEditor(id, view); paintVerification(id, view); };
    row.append(title, remove, check);
    content.append(row);
  });
}

function initVerificationControls() {
  $("#session-verification-view").onclick = () => {
    const selected = selectedChatSession();
    if (!selected) return;
    const view = verificationView(selected.id);
    view.open = !view.open;
    renderVerificationView();
  };
  $("#verification-close").onclick = () => {
    if (verificationShown) verificationView(verificationShown).open = false;
    renderVerificationView();
  };
  $("#verification-refresh").onclick = () => {
    if (verificationShown) readVerification(verificationShown, true).catch(error);
  };
}
