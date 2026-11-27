"use strict";

// Numeric snapshots only. One bounded reader serves both display modes.
const sessionUsageCache = new Map();
let sessionUsageTimer, sessionUsageReading = false, sessionUsageRound = 0;
let sessionUsageEpoch = 0, sessionUsagePopup = null;
let sessionUsagePreferences = Promise.resolve();

function usageNumber(value) {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= Number.MAX_SAFE_INTEGER ? value : null;
}
function usageThresholds(preferences = db.preferences || {}) {
  const tokens = usageNumber(preferences.usageTokenWarning), context = usageNumber(preferences.usageContextWarning);
  return { tokens:tokens > 0 ? tokens : null, context:context > 0 && context <= 100 ? context : null };
}
function usageWarnings(data, preferences) {
  const limits = usageThresholds(preferences);
  const total = usageNumber(data?.tokens?.totalTokens), percent = usageNumber(data?.context?.usedPercent);
  return { tokens:limits.tokens !== null && total !== null && total > limits.tokens,
    context:limits.context !== null && percent !== null && percent > limits.context };
}
function usageScope(id) {
  const owner = session(id);
  if (!owner || owner.archived || owner.login) return null;
  return JSON.stringify([owner.id,owner.provider,owner.accountId || null,owner.directory,
    owner.conversationId || null,owner.runId || null,owner.model || null,db.active.find(item=>item.id===id)?.runId || null]);
}
function usageEntry(id) {
  const scope = usageScope(id);
  if (!scope) { sessionUsageCache.delete(id); return null; }
  let entry = sessionUsageCache.get(id);
  if (!entry || entry.scope !== scope) entry = {scope,data:null,readAt:0};
  sessionUsageCache.delete(id); sessionUsageCache.set(id,entry);
  while (sessionUsageCache.size > 16) sessionUsageCache.delete(sessionUsageCache.keys().next().value);
  return entry;
}
function cachedSessionUsage(id) {
  const entry=sessionUsageCache.get(id);
  return entry?.scope===usageScope(id)?entry:null;
}
function usageCompact(value) {
  const number = usageNumber(value);
  if(number === null) return "—";
  const format = amount => amount.toLocaleString(uiLanguage,{maximumFractionDigits:1});
  return number >= 1000000 ? format(number/1000000)+"M" : number >= 1000 ? format(number/1000)+"k" : format(number);
}
function usageExact(value) {
  return usageNumber(value) === null ? "—" : value.toLocaleString(uiLanguage,{maximumFractionDigits:2});
}
function newSessionUsageButton(id) {
  const meter = el("button","session-usage-meter");
  meter.type = "button"; meter.dataset.sessionUsage = id;
  meter.setAttribute("aria-haspopup","dialog");
  meter.setAttribute("aria-controls","session-usage-popover");
  meter.append(el("span","usage-warning-icon","⚠"),el("span","usage-meter-label"),el("span","usage-meter-track"),el("span","usage-meter-value"));
  meter.querySelector(".usage-meter-track").append(el("span","usage-meter-fill"));
  meter.querySelector(".usage-warning-icon").setAttribute("aria-hidden","true");
  meter.querySelector(".usage-meter-track").setAttribute("aria-hidden","true");
  meter.onclick = event => {event.stopPropagation();openSessionUsage(id,meter);};
  return meter;
}
function renderSessionUsage() {
  const slot = $("#chat-session-usage");
  if(slot) {
    const selected = shownChatId && session(shownChatId);
    slot.hidden = !selected;
    if(selected && slot.firstElementChild?.dataset.sessionUsage !== selected.id) slot.replaceChildren(newSessionUsageButton(selected.id));
    else if(!selected) slot.replaceChildren();
  }
  for(const meter of $$('[data-session-usage]')) {
    const id=meter.dataset.sessionUsage, entry=cachedSessionUsage(id), data=entry?.data;
    const warnings=usageWarnings(data), warning=warnings.tokens || warnings.context;
    const percent=usageNumber(data?.context?.usedPercent), used=usageNumber(data?.context?.usedTokens), total=usageNumber(data?.tokens?.totalTokens);
    meter.hidden=!usageScope(id) || (db.preferences?.sessionUsageVisible===false && !warning);
    meter.classList.toggle("usage-warning",warning);
    meter.classList.toggle("usage-context-warning",warnings.context);
    meter.classList.toggle("usage-warning-only",db.preferences?.sessionUsageVisible===false && warning);
    const label=warning ? tr(warnings.tokens && warnings.context ? "Kullanım eşikleri aşıldı" : warnings.tokens ? "Token eşiği aşıldı" : "Bağlam eşiği aşıldı")
      : tr(percent!==null ? "Bağlam" : used!==null ? (data.context.basis==="last-request-input" ? "Son girdi" : "Bağlam") : total!==null ? "Tokenlar" : "Kullanım");
    const tokenWarning=warnings.tokens && !warnings.context;
    const value=tokenWarning?usageCompact(total):percent!==null ? "%"+usageExact(percent) : usageCompact(used ?? total);
    meter.querySelector(".usage-meter-label").textContent=label;
    meter.querySelector(".usage-meter-value").textContent=value;
    meter.querySelector(".usage-warning-icon").hidden=!warning;
    meter.querySelector(".usage-meter-track").hidden=percent===null || tokenWarning;
    meter.style.setProperty("--session-context-percent",Math.min(100,percent || 0)+"%");
    meter.setAttribute("aria-label",label+" · "+value+" · "+tr("Bağlam ve token ayrıntılarını aç"));
    meter.setAttribute("aria-expanded",String(sessionUsagePopup?.anchor===meter));
    meter.title=label+" · "+value;
  }
  if(sessionUsagePopup) {
    if(!sessionUsagePopup.anchor.isConnected || sessionUsagePopup.scope!==usageScope(sessionUsagePopup.id) || sessionUsagePopup.anchor.hidden) closeSessionUsage();
    else paintSessionUsagePopover();
  }
}
async function readSessionUsage(id) {
  const entry=usageEntry(id);
  if(!entry) return;
  const epoch=sessionUsageEpoch, scope=entry.scope;
  try {
    const data=await api("session_usage","read",{id});
    if(epoch!==sessionUsageEpoch || sessionUsageCache.get(id)!==entry || scope!==usageScope(id)) return;
    const owner=session(id);
    if(data.sessionId!==id || data.provider!==owner.provider || (data.conversationId || null)!==(owner.conversationId || null)
      || (data.accountId || null)!==(owner.accountId || null) || (data.runId || null)!==(owner.runId || null)) return;
    entry.data=data;
  } catch {
    if(epoch!==sessionUsageEpoch || sessionUsageCache.get(id)!==entry || scope!==usageScope(id)) return;
    entry.data=null;
  }
  entry.readAt=Date.now();
  renderSessionUsage();
}
function scheduleSessionUsage(immediate = false) {
  renderSessionUsage();
  if(immediate) {clearTimeout(sessionUsageTimer);sessionUsageTimer=undefined;}
  if(sessionUsageTimer!==undefined || sessionUsageReading) return;
  sessionUsageTimer=setTimeout(pollSessionUsage,immediate?0:2500);
}
async function pollSessionUsage() {
  sessionUsageTimer=undefined;
  if(sessionUsageReading) return;
  sessionUsageReading=true;
  try {
    const limits=usageThresholds();
    if(document.visibilityState!=="visible" || (db.preferences?.sessionUsageVisible===false && limits.tokens===null && limits.context===null)) return;
    const selected=sessionUsagePopup?.id || shownChatId || focusId, owner=selected && session(selected);
    if(owner && usageScope(selected)) {
      const entry=usageEntry(selected);
      if(sessionUsagePopup || active(selected) || Date.now()-entry.readAt>10000) await readSessionUsage(selected);
    }
    const others=[...views.keys()].filter(id=>id!==selected && usageScope(id))
      .filter(id=>active(id) || !cachedSessionUsage(id)?.readAt || Date.now()-cachedSessionUsage(id).readAt>30000);
    if(others.length) await readSessionUsage(others[sessionUsageRound++ % others.length]);
  } finally {
    sessionUsageReading=false;
    scheduleSessionUsage();
  }
}
function closeSessionUsage() {
  const panel=$("#session-usage-popover");
  if(panel?.matches(":popover-open")) panel.hidePopover();
  sessionUsagePopup?.anchor.setAttribute("aria-expanded","false");
  sessionUsagePopup=null;
}
function openSessionUsage(id,anchor) {
  if(sessionUsagePopup?.anchor===anchor) {closeSessionUsage();return;}
  closeSessionUsage();
  sessionUsagePopup={id,anchor,scope:usageScope(id),painted:null};
  paintSessionUsagePopover();
  $("#session-usage-popover").showPopover();
  positionSessionUsage();
  anchor.setAttribute("aria-expanded","true");
  scheduleSessionUsage(true);
}
function positionSessionUsage() {
  if(!sessionUsagePopup) return;
  const panel=$("#session-usage-popover"), rect=sessionUsagePopup.anchor.getBoundingClientRect();
  const box=panel.getBoundingClientRect();
  panel.style.left=Math.max(10,Math.min(innerWidth-box.width-10,rect.right-box.width))+"px";
  panel.style.top=Math.max(10,Math.min(innerHeight-box.height-10,rect.top-box.height-8))+"px";
}
function paintSessionUsagePopover() {
  if(!sessionUsagePopup) return;
  const {id}=sessionUsagePopup, entry=cachedSessionUsage(id), data=entry?.data;
  const key=JSON.stringify([data,db.preferences?.sessionUsageVisible,usageThresholds()]);
  if(sessionUsagePopup.painted===key) return;
  sessionUsagePopup.painted=key;
  const panel=$("#session-usage-popover"), context=data?.context, tokens=data?.tokens, warnings=usageWarnings(data);
  const head=el("div","usage-detail-head");
  head.append(el("strong","",tr("Bağlam ve tokenlar")),button("×",closeSessionUsage,"icon quiet",tr("Kapat")));
  const body=el("div","usage-detail-body");
  function row(label,value) {const line=el("div","usage-detail-row");line.append(el("span","",tr(label)),el("bdi","",value));body.append(line);}
  row("Bağlam doluluğu",usageNumber(context?.usedPercent)===null?"—":"%"+usageExact(context.usedPercent));
  row(context?.basis==="last-request-input"?"Son isteğin girdisi":"Bağlam tokenı",usageExact(context?.usedTokens));
  row("Bağlam kapasitesi",usageExact(context?.limitTokens));
  if(usageNumber(context?.usedPercent)!==null) body.append(el("p","help",tr("Bağlam oranı, ölçülen tokenın bildirilen kapasiteye bölünmesiyle hesaplanır.")));
  body.append(el("div","usage-detail-divider"),el("small","help",tr("TOKEN · OTURUM TOPLAMI")));
  for(const [label,key] of [["Girdi","inputTokens"],["Çıktı","outputTokens"],["Önbellekten okunan","cachedInputTokens"],["Toplam token","totalTokens"]]) row(label,usageExact(tokens?.[key]));
  const note=el("p","help",tr(data?.available?"Yalnızca sağlayıcının ölçtüğü değerler gösterilir. Alınamayan alanlar boş bırakılır.":"Bu konuşma için sağlayıcıdan kullanım bilgisi alınamadı."));
  if(data?.observedAt && !Number.isNaN(Date.parse(data.observedAt))) note.textContent+=" "+tr("Son okuma: {time}",{time:new Date(data.observedAt).toLocaleTimeString(uiLanguage)});
  if(warnings.tokens || warnings.context) body.append(el("p","usage-threshold-notice",tr(warnings.tokens && warnings.context?"Token ve bağlam eşikleri aşıldı.":warnings.tokens?"Oturum token eşiği aşıldı.":"Bağlam doluluk eşiği aşıldı.")));
  const foot=el("div","usage-detail-actions");
  foot.append(button(tr("Göstergeyi gizle"),()=>saveSessionUsagePreferences({sessionUsageVisible:false}).then(closeSessionUsage).catch(error),"quiet"));
  panel.replaceChildren(head,body,note,foot);
  if(panel.matches(":popover-open")) positionSessionUsage();
}
async function saveSessionUsagePreferences(values) {
  const request=sessionUsagePreferences.catch(()=>{}).then(()=>api("app","preferences",values));
  sessionUsagePreferences=request;
  db=await request;
  renderSessionUsage();
  scheduleSessionUsage(true);
}
function openSessionUsageSettings() {
  modal(tr("Oturum kullanımı"),sessionUsageSettings());
  $("#dialog-footer button").textContent=tr("Kapat");
}
function sessionUsageSettings() {
  const box=el("section","session-usage-settings");box.id="session-usage-settings";
  const visible=el("input");visible.type="checkbox";visible.id="session-usage-visible";visible.checked=db.preferences?.sessionUsageVisible!==false;
  const toggle=el("label","update-toggle");toggle.append(visible,document.createTextNode(tr("Bağlam ve token göstergesini göster")));
  const limits=usageThresholds(), tokens=input(limits.tokens??""), context=input(limits.context??"");
  tokens.type=context.type="number";tokens.min="1";tokens.max=String(Number.MAX_SAFE_INTEGER);tokens.step="1";context.min="0";context.max="100";context.step="any";
  tokens.id="usage-token-warning";context.id="usage-context-warning";
  tokens.placeholder=context.placeholder=tr("Uyarı yok");
  const status=el("p","help");status.setAttribute("role","status");
  visible.onchange=async()=>{visible.disabled=true;try{await saveSessionUsagePreferences({sessionUsageVisible:visible.checked});}catch(e){visible.checked=db.preferences?.sessionUsageVisible!==false;error(e);}finally{visible.disabled=false;}};
  const saveButton=button(tr("Uyarı eşiklerini kaydet"),()=>{});
  saveButton.id="save-usage-thresholds";
  saveButton.onclick=busy(saveButton,async()=>{
    if(tokens.validity.badInput || context.validity.badInput) throw Error(tr("Geçerli bir sayı gir."));
    const tokenValue=tokens.value.trim()===""?null:Number(tokens.value), contextValue=context.value.trim()===""?null:Number(context.value);
    if(tokenValue!==null && (!Number.isSafeInteger(tokenValue) || tokenValue<=0)) throw Error(tr("Token eşiği pozitif bir tam sayı olmalı."));
    if(contextValue!==null && (!Number.isFinite(contextValue) || contextValue<=0 || contextValue>100)) throw Error(tr("Bağlam eşiği 0’dan büyük, en fazla 100 olmalı."));
    await saveSessionUsagePreferences({usageTokenWarning:tokenValue,usageContextWarning:contextValue});
    status.textContent=tr("Uyarı eşikleri kaydedildi.");
  });
  box.append(toggle,field(tr("Oturum toplam token eşiği"),tokens),field(tr("Bağlam doluluk eşiği (%)"),context),
    el("p","help",tr("Boş alanlar uyarı üretmez. Ölçülen değerlerden biri eşiğini aşarsa oturumda uyarı görünür. Göstergeyi gizlemek bu uyarıları kapatmaz.")),saveButton,status);
  return box;
}
function resetSessionUsage() {
  sessionUsageEpoch++;
  clearTimeout(sessionUsageTimer);sessionUsageTimer=undefined;
  sessionUsageCache.clear();closeSessionUsage();
}
function initSessionUsage() {
  $("#session-usage-popover").addEventListener("toggle",event=>{if(event.newState==="closed" && !event.target.matches(":popover-open") && sessionUsagePopup) {sessionUsagePopup.anchor.setAttribute("aria-expanded","false");sessionUsagePopup=null;}});
  window.addEventListener("resize",positionSessionUsage);
  document.addEventListener("visibilitychange",()=>scheduleSessionUsage(true));
}
