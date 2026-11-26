"use strict";

function formatStorageSize(bytes) {
  const value=Math.max(0,Number(bytes)||0);
  if(value<1024) return value+" B";
  if(value<1024*1024) return (value/1024).toFixed(1)+" KiB";
  return (value/(1024*1024)).toFixed(1)+" MiB";
}

function resetClientLayout(eraseDrafts=false) {
  for(const id of [...views.keys()]) disposeTerminalView(id);
  for(const card of sessionCards.values()) card.remove();
  sessionCards.clear();
  for(const view of chatViews.values()) view.open=false;
  if(eraseDrafts) {
    resetVerificationViews();
    resetSessionUsage();
    chatViews.clear();
    $("#chat-turns").replaceChildren();
    $("#chat-input").value="";
    $("#chat-terminal-output").textContent="";
    $("#chat-images")?.replaceChildren();
    $("#chat-terminal-prompt-text").replaceChildren();
    $("#chat-terminal-choices").replaceChildren();
    $("#chat-terminal-prompt").hidden=true;
  }
  for(const key of Object.keys(projectFocus)) delete projectFocus[key];
  for(const key of Object.keys(projectFolders)) delete projectFolders[key];
  for(const key of Object.keys(folderLayout)) delete folderLayout[key];
  collapsedSessionFolders.clear();
  for(const key of ["agentloom-layout","agentloom-widths","agentloom-sections"]) localStorage.removeItem(key);
  for(const side of Object.values(sidePanels)) document.documentElement.style.removeProperty(side.variable);
  for(const handle of $$("[data-resize]")) $("#"+handle.dataset.resize)?.style.removeProperty("height");
  projectId=folderId=focusId=null;
  mode="map"; camera={x:25,y:40,z:0.75}; panel="browser";
  changesPreference="git";
  document.body.classList.remove("no-left","no-right","grid","focus");
  $("#providers-section").hidden=true;
}

async function openDataManagement() {
  const box=el("div","data-management");
  box.id="data-management";
  box.textContent=tr("Yükleniyor…");
  modal(tr("Veri yönetimi"),box);
  $("#dialog-footer button").textContent=tr("Kapat");
  async function refresh() {
    const status=await api("data_management","status");
    if(!box.isConnected || !$("#dialog").open) return;
    const stats=el("dl","data-stats");
    for(const [label,value] of [
      ["Kayıtlı proje",status.projects],["Oturum",status.sessions],["Arşivlenmiş oturum",status.archived],
      ["Yüklü terminal görünümü",views.size+" / "+MAX_TERMINAL_VIEWS],
      ["Bellekteki terminal süreçleri",status.retainedTerminals],
      ["Çalışma alanı kaydı",formatStorageSize(status.workspaceBytes)],
      ["Kaydedilmiş terminal çıktısı",formatStorageSize(status.terminalBytes)]]) {
      stats.append(el("dt","",tr(label)),el("dd","",value));
    }
    const help=el("p","help",tr("Arşivler otomatik yüklenmez. Terminal görünümleri gerektiğinde, konuşma geçmişi sayfalar halinde açılır."));
    const memory=button(tr("Önbellekleri boşalt"),()=>{});
    memory.id="data-clear-memory";
    memory.onclick=busy(memory,async()=>{
      await api("data_management","clear_memory");
      renderChatView();
      toast(tr("Önbellekler ve geçici tarayıcı hata görüntüleri boşaltıldı. Hesaplar, taslaklar ve geçmiş kayıtları korundu."));
      await refresh();
    });
    const layout=button(tr("Görünüm düzenini sıfırla"),()=>{
      modal(tr("Görünüm düzenini sıfırla"),el("p","help",tr("Kart konumları ve panel düzeni varsayılana döner. Hesaplar, oturumlar, taslaklar ve proje dosyaları korunur. Çalışan terminaller durdurulmaz.")),async()=>{
        await api("data_management","reset_layout");
        resetClientLayout();
        await sync(); setMode("map"); await openPanel("browser");
      },tr("Sıfırla"));
    });
    layout.id="data-reset-layout";
    const reset=button(tr("Proje ve oturum kayıtlarını sıfırla"),()=>confirmReset(status),"danger");
    reset.id="data-reset-records"; reset.disabled=!status.hasRecords && !status.managedFolders;
    const resetHelp=el("p","help",tr("Kayıtlar önce yedeklenir. İstersen sonraki ekranda Workflow kopyalarını da ayrı onayla silebilirsin. Ana kaynak klasörleri, hesaplar ve sağlayıcı konuşmaları korunur."));
    const actions=el("div","data-actions"); actions.append(memory,layout);
    const records=el("section","data-records"); records.append(resetHelp,reset);
    if(status.lastReset?.id && !status.lastReset.destructive && !status.pendingCleanup && !status.hasRecords) {
      const restore=button(tr("Son sıfırlamayı geri al"),()=>{
        modal(tr("Son sıfırlamayı geri al"),el("p","help",tr("Yerel yedekteki proje ve oturum kayıtları geri gelir. Dosyalar değiştirilmez ve hiçbir sağlayıcı başlatılmaz. Bu işlem yalnızca liste boşken kullanılabilir.")),async()=>{
          await api("data_management","restore_records",{confirmation:"RESTORE"});
          resetClientLayout(); await sync(); setMode("map");
        },tr("Geri yükle"));
      });
      restore.id="data-restore-records"; records.append(restore);
    }
    const reload=button(tr("Yenile"),refresh,"quiet");
    box.replaceChildren(stats,help,actions,records,reload);
    if(status.pendingCleanup) records.prepend(el("p","dialog-error",tr("Önceki dosya temizliği yarım kaldı. Bazı kopyalar silinmiş olabilir. Sıfırlama ekranında dosya temizliğini seçip güncel önizlemeyi onaylayarak tamamlayabilirsin.")));
    if(status.limited) box.append(el("p","help",tr("Boyut hesabı dosya sınırında durdu; gösterilen değer en az bu kadardır.")));
  }
  function confirmReset(status) {
    const content=el("div");
    content.append(el("p","",tr("{projects} proje ve {sessions} oturum kaydı çalışma alanından kaldırılacak.",{projects:status.projects,sessions:status.sessions})),
      el("p","help",tr("Önce terminalleri, hesap girişini, önizlemeleri ve dahili tarayıcı sekmelerini kapat. Gönderilmemiş taslaklar kapanır. Hesaplar ve diskteki dosyalar korunur.")));
    const deleteFiles=el("input"); deleteFiles.type="checkbox"; deleteFiles.id="data-delete-workspaces";
    const deleteLabel=el("label","update-toggle");
    deleteLabel.append(deleteFiles,document.createTextNode(tr("Workflow'un ayrı çalışma alanlarını ve arşiv kopyalarını da sil")));
    const previewBox=el("div","cleanup-preview"); previewBox.hidden=true;
    const confirmation=el("input"); confirmation.id="data-reset-confirmation";
    confirmation.autocomplete="off";
    const required=el("span","",tr("Onaylamak için SIFIRLA yaz"));
    const confirmLabel=el("label","form-field"); confirmLabel.append(required,confirmation);
    content.append(deleteLabel,previewBox,confirmLabel);
    let preview=null,loading=false,generation=0;
    const expected=()=>deleteFiles.checked?"SIL VE SIFIRLA":"SIFIRLA";
    modal(tr("Proje ve oturum kayıtlarını sıfırla"),content,async()=>{
      if(confirmation.value.trim()!==expected() || loading) throw Error(required.textContent);
      if(deleteFiles.checked && (!preview || preview.blocked.length)) throw Error(tr("Önce dosya temizleme önizlemesini tamamla."));
      const result=await api("data_management","reset_records",{revision:status.revision,confirmation:"RESET",
        ...(deleteFiles.checked?{deleteWorkspaces:true,fileConfirmation:"DELETE",cleanupFingerprint:preview.fingerprint}:{})});
      resetClientLayout(true); await sync(); setMode("map");
      toast(tr(result.filesRemoved
        ? "Kayıtlar ve Workflow kopyaları temizlendi. Ana kaynak klasörleri ve hesaplar korundu."
        : "Kayıtlar yedeklenerek sıfırlandı. Veri yönetiminden geri alabilirsin."));
    },tr("Yedekle ve sıfırla"));
    const apply=$("#dialog-footer .primary");
    function controls() {
      required.textContent=tr(deleteFiles.checked?"Onaylamak için SIL VE SIFIRLA yaz":"Onaylamak için SIFIRLA yaz");
      apply.textContent=tr(deleteFiles.checked?"Kopyaları sil ve sıfırla":"Yedekle ve sıfırla");
      apply.disabled=loading || confirmation.value.trim()!==expected()
        || (deleteFiles.checked && (!preview || preview.blocked.length || (!preview.folders.length && !status.pendingCleanup)))
        || (!deleteFiles.checked && (!status.hasRecords || status.pendingCleanup));
    }
    confirmation.oninput=controls;
    deleteFiles.onchange=async()=>{
      const revision=++generation;
      confirmation.value=""; preview=null; loading=deleteFiles.checked;
      previewBox.hidden=!deleteFiles.checked;
      previewBox.replaceChildren(el("p","help",tr("Klasörler inceleniyor…"))); controls();
      if(!deleteFiles.checked) return;
      try {
        const result=await api("data_management","cleanup_preview");
        if(revision!==generation || !content.isConnected) return;
        preview=result;
        const summary=el("p","",tr("{folders} ayrı klasör · {files} dosya · {size}",{
          folders:result.folders.length,files:result.files,size:formatStorageSize(result.bytes)}));
        previewBox.replaceChildren(summary,el("p","help",tr("Bu kopyalardaki birleştirilmemiş değişiklikler de kalıcı olarak silinir. Dosyalar yedeğe alınmaz; bu seçim geri alınamaz. Ana kaynak kod ve Git dalları korunur.")));
        const list=el("ul","cleanup-targets");
        for(const folder of result.folders) {
          const row=el("li"); row.append(el("strong","",folder.name),el("small","",folder.path)); list.append(row);
        }
        previewBox.append(list);
        for(const issue of result.blocked) {
          const row=el("p","dialog-error"); row.textContent=issue.path+" · "+issue.reason; previewBox.append(row);
        }
        if(!result.folders.length) previewBox.append(el("p","help",tr("Silinebilecek doğrulanmış ayrı çalışma alanı yok.")));
      } catch(e) { if(revision===generation && content.isConnected) previewBox.replaceChildren(el("p","dialog-error",uiError(e))); }
      finally { if(revision===generation) { loading=false; controls(); } }
    };
    controls();
  }
  await refresh();
}
