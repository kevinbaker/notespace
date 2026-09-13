// The reply form's quote buttons. Without this script the parent post is there to read and
// the textarea is there to type in; nothing is rendered that only works with it.
(function () {
  var parent = document.getElementById('parent');
  var body = document.getElementById('body');
  var actions = parent && parent.querySelector('.parent-actions');
  var content = parent && parent.querySelector('.post-body');
  if (!parent || !body || !actions || !content) return;

  function selectedInParent() {
    var sel = window.getSelection();
    if (!sel || sel.isCollapsed || sel.rangeCount === 0) return '';
    var range = sel.getRangeAt(0);
    if (!content.contains(range.commonAncestorContainer)) return '';
    return sel.toString();
  }

  // Inserts at the cursor, on its own paragraph, and leaves the cursor after it.
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

  function button(label, onClick) {
    var b = document.createElement('button');
    b.type = 'button';
    b.className = 'quote';
    b.textContent = label;
    b.addEventListener('click', onClick);
    actions.appendChild(document.createTextNode(' · '));
    actions.appendChild(b);
    return b;
  }

  var selection = button('quote selection', function () {
    var text = selectedInParent();
    if (text) quote(text);
  });
  button('quote all', function () { quote(content.innerText); });

  // The selection button only lights up while something in the parent is selected.
  function refresh() { selection.disabled = !selectedInParent(); }
  document.addEventListener('selectionchange', refresh);
  refresh();
})();
