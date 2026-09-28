// WebDriver has no CDP accessibility tree. Record semantic DOM nodes and stable
// element identities without modifying the page; CSS paths are descriptive.
// W3C serializes each element below to a session-bound element reference.
const __agentBrowserSnapshot = [];
const root = options.selector ? document.querySelector(options.selector) : document.body;
if (!root) throw new Error('Snapshot root not found');
function path(e) {
  if (e.id && document.querySelectorAll('#' + CSS.escape(e.id)).length === 1) return '#' + CSS.escape(e.id);
  const parts = [];
  while (e && e.nodeType === 1) {
    const siblings = e.parentElement ? Array.from(e.parentElement.children) : [e];
    parts.unshift(e.localName + ':nth-child(' + (siblings.indexOf(e) + 1) + ')');
    e = e.parentElement;
  }
  return parts.join(' > ');
}
function visit(e, depth) {
  if (options.maxDepth != null && depth > options.maxDepth) return;
  const style = getComputedStyle(e);
  const hasRects = e.getClientRects().length > 0;
  // display:contents has no box, but its visible descendants still participate.
  if ((!hasRects && style.display !== 'contents') || style.visibility === 'hidden' || e.getAttribute('aria-hidden') === 'true') return;
  const tag = e.localName;
  const roles = {button:'button',a:e.hasAttribute('href') ? 'link' : '',input:e.type === 'checkbox' ? 'checkbox' : e.type === 'radio' ? 'radio' : e.type === 'button' || e.type === 'submit' ? 'button' : 'textbox',textarea:'textbox',select:'combobox',img:'img'};
  const role = e.getAttribute('role') || roles[tag] || (/^h[1-6]$/.test(tag) ? 'heading' : '');
  const interactive = /^(button|link|textbox|checkbox|radio|combobox|switch|slider|menuitem)$/.test(role);
  if (hasRects && role && (!options.interactive || interactive)) {
    const labelled = (e.getAttribute('aria-labelledby') || '').split(/\s+/).map(id => document.getElementById(id)?.textContent || '').join(' ').trim();
    const name = e.getAttribute('aria-label') || labelled || Array.from(e.labels || []).map(l => l.textContent).join(' ') || e.getAttribute('alt') || e.getAttribute('title') || e.getAttribute('placeholder') || e.innerText || e.value || '';
    __agentBrowserSnapshot.push({role,name:name.trim().replace(/\s+/g,' '),selector:path(e),element:e,depth,interactive});
  }
  for (const child of e.children) visit(child, depth + 1);
}
visit(root, 0);
return __agentBrowserSnapshot;
