"use strict";

async function openAtlassianConnections() {
  const box = el("div", "atlassian-connections");
  box.append(el("p", "help", tr("Jira ve Confluence Data Center bağlantıları yalnızca okuma ve arama içindir.")),
    el("p", "help", tr("MCP açık oturumlar bu bağlantıları kullanabilir. Okunan içerik oturumda seçtiğin modele iletilir; token iletilmez. Token güvenli sistem deposunda saklanır.")));
  modal(tr("Bağlantılar"), box);
  $("#dialog").addEventListener("close", () => {
    for (const field of box.querySelectorAll('input[type="password"]')) field.value = "";
  }, {once:true});
  $("#dialog-footer button").textContent = tr("Kapat");
  const statuses = await api("atlassian", "status");
  if (!box.isConnected) return;
  for (const [kind, name] of [["jira", "Jira"], ["confluence", "Confluence"]]) {
    let saved = statuses[kind];
    const card = el("section", "atlassian-connection"), title = el("strong", "", name);
    card.dataset.kind = kind;
    const address = input(saved.baseUrl || "");
    address.type = "url"; address.maxLength = 1024; address.autocomplete = "off";
    address.placeholder = "https://service.example.invalid/" + kind;
    address.setAttribute("aria-label", name + " " + tr("Sunucu adresi"));
    const token = input(""); token.type = "password"; token.maxLength = 1024;
    token.autocomplete = "new-password"; token.spellcheck = false;
    token.setAttribute("aria-label", name + " " + tr("Kişisel erişim token’ı"));
    token.placeholder = tr(saved.configured ? "Boş: kayıtlı token korunur" : "Kişisel erişim token’ı");
    const status = el("p", "help"), actions = el("div", "atlassian-actions");
    const paint = () => {
      status.textContent = tr(saved.unavailable ? "Güvenli token deposuna erişilemiyor." : saved.configured ? "Bağlantı kayıtlı · Salt okunur" : "Bağlantı yok");
      remove.disabled = !saved.configured && !saved.unavailable;
    };
    const act = async action => {
      for (const b of actions.children) b.disabled = true;
      address.disabled = token.disabled = true;
      try {
        const result = await api("atlassian", action, { kind, baseUrl: address.value.trim(), token: token.value });
        if (!card.isConnected) return;
        if (action === "remove") { saved = {configured:false}; address.value = ""; }
        else if (action === "save") saved = result.connection;
        paint(); status.textContent = tr(action === "test" ? "Bağlantı testi başarılı." : action === "save" ? "Bağlantı kaydedildi." : "Bağlantı kaldırıldı.");
        status.className = "help";
      } catch (e) {
        if (card.isConnected) { status.textContent = uiError(e); status.className = "dialog-error"; }
      } finally {
        if (action !== "test") token.value = "";
        if (card.isConnected) {
          for (const b of actions.children) b.disabled = false;
          remove.disabled = !saved.configured && !saved.unavailable;
          address.disabled = token.disabled = false;
          token.placeholder = tr(saved.configured ? "Boş: kayıtlı token korunur" : "Kişisel erişim token’ı");
        }
      }
    };
    const test = button(tr("Test et"), () => act("test")), save = button(tr("Kaydet"), () => act("save"), "primary"), remove = button(tr("Bağlantıyı kaldır"), () => act("remove"));
    actions.append(test, save, remove);
    card.append(title, field(tr("Sunucu adresi"), address), field(tr("Kişisel erişim token’ı"), token), status, actions);
    box.append(card); paint();
  }
}
