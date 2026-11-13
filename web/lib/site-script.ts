// The product pages' only own script, inlined first in <body> (postbuild hashes it for the CSP),
// so these pages load no theme library:
//
//   1. theme   before paint: `dark` on <html> from localStorage.theme (system, light, dark; the
//              key the docs' provider uses) and the system preference
//   2. control clicks and arrow keys on the [data-theme-set] radios store the choice;
//              aria-checked marks it, also after a client-side page change (a mutation observer)
//   3. copy    clicks on button[data-copy] copy its text and say `Copied` in the live region
//   4. search  `/` (outside a field) or Ctrl/Cmd+K follows the [data-search] link
//
// Plain ES2017 in a string: it runs before any bundle, and event handlers are delegated to the
// document, so the markup carries no inline handler (the CSP forbids them).
export const SITE_SCRIPT = `(function () {
  var doc = document;
  var root = doc.documentElement;
  var KEY = 'theme';
  var THEMES = ['system', 'light', 'dark'];
  var query = window.matchMedia('(prefers-color-scheme: dark)');

  function stored() {
    try {
      var value = localStorage.getItem(KEY);
      return THEMES.indexOf(value) < 0 ? 'system' : value;
    } catch (e) {
      return 'system';
    }
  }

  function mark(theme) {
    var buttons = doc.querySelectorAll('[data-theme-set]');
    for (var i = 0; i < buttons.length; i++) {
      var on = buttons[i].getAttribute('data-theme-set') === theme;
      buttons[i].setAttribute('aria-checked', on ? 'true' : 'false');
      buttons[i].tabIndex = on ? 0 : -1;
    }
  }

  function apply(theme) {
    var dark = theme === 'dark' || (theme === 'system' && query.matches);
    root.classList.toggle('dark', dark);
    root.style.colorScheme = dark ? 'dark' : 'light';
    mark(theme);
  }

  apply(stored());
  query.addEventListener('change', function () {
    if (stored() === 'system') {
      apply('system');
    }
  });
  // The control renders after this script, and again on a client-side page change: mark it then.
  new MutationObserver(function () {
    mark(stored());
  }).observe(root, { childList: true, subtree: true });

  // Arrow keys move the checked theme along the radios, Home and End to the first and last.
  var STEP = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
  function choose(radio, key) {
    var radios = radio.parentElement.querySelectorAll('[data-theme-set]');
    var at = Array.prototype.indexOf.call(radios, radio);
    var next = key === 'Home' ? 0 : key === 'End' ? radios.length - 1 : (at + STEP[key] + radios.length) % radios.length;
    var theme = radios[next].getAttribute('data-theme-set');
    try { localStorage.setItem(KEY, theme); } catch (e) {}
    apply(theme);
    radios[next].focus();
  }

  function copy(button) {
    var region = button.parentElement && button.parentElement.querySelector('[aria-live="polite"]');
    var say = function (text) {
      if (region) {
        region.textContent = text;
        setTimeout(function () { region.textContent = ''; }, 2000);
      }
    };
    if (!navigator.clipboard) {
      say('Copy failed: select the line');
      return;
    }
    navigator.clipboard.writeText(button.getAttribute('data-copy')).then(
      function () { say('Copied'); },
      function () { say('Copy failed: select the line'); }
    );
  }

  doc.addEventListener('click', function (event) {
    var target = event.target;
    if (!target || !target.closest) {
      return;
    }
    var set = target.closest('[data-theme-set]');
    if (set) {
      var theme = set.getAttribute('data-theme-set');
      try { localStorage.setItem(KEY, theme); } catch (e) {}
      apply(theme);
      return;
    }
    var button = target.closest('button[data-copy]');
    if (button) {
      copy(button);
    }
  });

  doc.addEventListener('keydown', function (event) {
    var target = event.target;
    var radio = target && target.closest && target.closest('[data-theme-set]');
    var moves = Object.prototype.hasOwnProperty.call(STEP, event.key) || event.key === 'Home' || event.key === 'End';
    if (radio && moves) {
      event.preventDefault();
      choose(radio, event.key);
      return;
    }
    var typing = target && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));
    var slash = event.key === '/' && !typing && !event.ctrlKey && !event.metaKey && !event.altKey;
    var combo = (event.ctrlKey || event.metaKey) && (event.key === 'k' || event.key === 'K');
    var link = doc.querySelector('a[data-search]');
    if (!link || !(slash || combo)) {
      return;
    }
    event.preventDefault();
    window.location.href = link.href;
  });
})();`;

