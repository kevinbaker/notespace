// Personalises the shared header. Pages are baked once for every reader, so "sign in" is what
// they all carry; this asks /api/me who the reader is and swaps the links -- but only when the
// ns_in marker cookie says there is a session to ask about, so anonymous readers cost nothing.
(function () {
  if (!/(^|; )ns_in=1(;|$)/.test(document.cookie)) return;
  var links = document.querySelector('header.site .links');
  if (!links) return;
  fetch('/api/me', { credentials: 'same-origin' })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (me) {
      if (!me || !me.name) return;
      var keep = [];
      links.querySelectorAll('a').forEach(function (a) {
        var p = a.getAttribute('href');
        if (p !== '/login' && p !== '/register' && p !== '/settings') keep.push(a);
      });
      links.textContent = '';
      keep.forEach(function (a) { links.appendChild(a); });
      if (me.moderator) {
        var q = document.createElement('a');
        q.href = '/mod/queue'; q.textContent = 'queue';
        links.appendChild(q);
      }
      var who = document.createElement('a');
      who.href = '/u/' + encodeURIComponent(me.name);
      who.textContent = me.name;
      who.className = 'me';
      links.appendChild(who);
      var acct = document.createElement('a');
      acct.href = '/settings'; acct.textContent = 'account';
      links.appendChild(acct);
      var out = document.createElement('form');
      out.method = 'post'; out.action = '/logout'; out.className = 'inline';
      var b = document.createElement('button');
      b.type = 'submit'; b.className = 'link'; b.textContent = 'sign out';
      out.appendChild(b);
      links.appendChild(out);
    })
    .catch(function () {});
})();
