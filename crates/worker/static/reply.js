// Progressive enhancement for the reply form: a "quote selection" button next to the
// no-JS "quote it all" link. With no selection inside the parent it quotes the whole post,
// which is what the link does; so the link is only ever a fallback, never a different feature.
(function () {
  var parent = document.getElementById('parent');
  var body = document.getElementById('body');
  var actions = parent && parent.querySelector('.parent-actions');
  if (!parent || !body || !actions) return;

  function selectedInParent() {
    var sel = window.getSelection();
    if (!sel || sel.isCollapsed || sel.rangeCount === 0) return '';
    var range = sel.getRangeAt(0);
    var content = parent.querySelector('.post-body');
    if (!content || !content.contains(range.commonAncestorContainer)) return '';
    return sel.toString();
  }

  function quote(text) {
    var lines = text.replace(/\s+$/, '').split(/\r?\n/);
    var q = lines.map(function (l) { return '> ' + l; }).join('\n') + '\n\n';
    var start = body.selectionStart, end = body.selectionEnd, v = body.value;
    var before = v.slice(0, start), after = v.slice(end);
    if (before && !/\n\n$/.test(before)) before += (/\n$/.test(before) ? '\n' : '\n\n');
    body.value = before + q + after;
    var at = (before + q).length;
    body.focus();
    body.setSelectionRange(at, at);
  }

  var button = document.createElement('button');
  button.type = 'button';
  button.className = 'quote';
  button.textContent = 'quote selection';
  button.addEventListener('click', function () {
    var text = selectedInParent() || parent.querySelector('.post-body').innerText;
    quote(text);
  });
  actions.appendChild(document.createTextNode(' · '));
  actions.appendChild(button);
})();
