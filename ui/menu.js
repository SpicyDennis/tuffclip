// Slate & Tally — shared File menu behaviour. Copy unchanged into ui/.
// Opens/closes/positions menus and handles the keyboard. Clicking an item
// fires a "menu-action" event on document with the item's data-action as detail.
(() => {
  let open = null; // { btn, menu }

  function close(focusBtn) {
    if (!open) return;
    open.menu.hidden = true;
    open.btn.setAttribute("aria-expanded", "false");
    if (focusBtn) open.btn.focus();
    open = null;
  }
  function items(menu) { return [...menu.querySelectorAll(".menu-item:not(:disabled)")]; }
  function show(btn, focusFirst) {
    close(false);
    const menu = document.getElementById(btn.dataset.menu);
    const r = btn.getBoundingClientRect();
    menu.style.left = r.left + "px";
    menu.style.top = r.bottom + 4 + "px";
    menu.hidden = false;
    btn.setAttribute("aria-expanded", "true");
    open = { btn, menu };
    if (focusFirst) items(menu)[0]?.focus();
  }

  document.addEventListener("click", (e) => {
    const btn = e.target.closest(".menu-btn");
    if (btn) { open?.btn === btn ? close(false) : show(btn, false); return; }
    const item = e.target.closest(".menu-item");
    if (item && open && open.menu.contains(item)) {
      close(false);
      document.dispatchEvent(new CustomEvent("menu-action", { detail: item.dataset.action }));
      return;
    }
    if (open && !open.menu.contains(e.target)) close(false);
  });

  document.addEventListener("keydown", (e) => {
    if (e.altKey && e.key.toLowerCase() === "f") {
      const btn = document.querySelector('.menu-btn[data-menu="file-menu"]');
      if (btn) { e.preventDefault(); show(btn, true); }
      return;
    }
    if (!open) return;
    const list = items(open.menu);
    const i = list.indexOf(document.activeElement);
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(true); }
    else if (e.key === "ArrowDown") { e.preventDefault(); list[(i + 1) % list.length]?.focus(); }
    else if (e.key === "ArrowUp") { e.preventDefault(); list[(i - 1 + list.length) % list.length]?.focus(); }
    else if (e.key === "Home") { e.preventDefault(); list[0]?.focus(); }
    else if (e.key === "End") { e.preventDefault(); list[list.length - 1]?.focus(); }
  }, true);

  window.addEventListener("blur", () => close(false));
  window.addEventListener("resize", () => close(false));
})();
