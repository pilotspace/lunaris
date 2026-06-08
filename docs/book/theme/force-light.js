// Light-only site guard.
//
// The book was previously dark-default ("navy"). A returning visitor can still
// carry `mdbook-theme=navy` (or coal/ayu/rust) in localStorage from that era —
// and since the theme switcher is hidden (theme/custom.css §0), they would be
// trapped on a dark page with no way out. This script neutralises any stored
// dark preference and forces the light class on <html>.
(function () {
    try {
        localStorage.setItem("mdbook-theme", "light");
    } catch (e) {
        /* private mode / disabled storage — fall through to class fixup */
    }
    var html = document.documentElement;
    ["navy", "coal", "ayu", "rust"].forEach(function (t) {
        html.classList.remove(t);
    });
    if (!html.classList.contains("light")) {
        html.classList.add("light");
    }
})();
