// GitBook-style progressive enhancements for the Lunaris book.
//
// Injects, per page:
//   1. a right-hand "On this page" TOC built from the page's h2/h3 headings,
//      with scroll-spy active highlighting and a "Was this helpful?" widget;
//   2. a multi-column site footer.
//
// Pure enhancement: with JS disabled the page is plain mdBook. All layout is
// driven by CSS (theme/custom.css §11); this script only builds DOM. mdBook
// 0.5.x is multi-page (full reload per navigation), so a per-page
// DOMContentLoaded build is sufficient and idempotent guards are belt-only.
(function () {
    "use strict";

    function ready(fn) {
        if (document.readyState !== "loading") {
            fn();
        } else {
            document.addEventListener("DOMContentLoaded", fn);
        }
    }

    // Root-relative prefix ("", "../", "../../", …) derived from the custom.css
    // <link> mdBook writes at every page depth. Lets injected internal links
    // resolve from any nesting level (and under the GitHub Pages subpath).
    function rootPrefix() {
        var link = document.querySelector('link[href*="theme/custom-"]');
        if (!link) return "";
        return link.getAttribute("href").replace(/theme\/custom-[^"'/]*\.css.*$/, "");
    }

    function el(tag, cls, text) {
        var e = document.createElement(tag);
        if (cls) e.className = cls;
        if (text != null) e.textContent = text;
        return e;
    }

    ready(function () {
        var content = document.getElementById("mdbook-content");
        var main = content ? content.querySelector("main") : null;
        if (!content || !main) return;
        var root = rootPrefix();

        // ---- 1. "On this page" TOC -------------------------------------
        if (!document.querySelector(".lx-toc")) {
            var heads = main.querySelectorAll("h2[id], h3[id]");
            if (heads.length >= 2) {
                var aside = el("aside", "lx-toc");
                aside.setAttribute("aria-label", "On this page");
                aside.appendChild(el("p", "lx-toc-title", "On this page"));

                var ul = el("ul");
                var links = [];
                heads.forEach(function (h) {
                    var a = el("a", h.tagName === "H3" ? "lx-h3" : "lx-h2", h.textContent);
                    a.href = "#" + h.id;
                    var li = el("li");
                    li.appendChild(a);
                    ul.appendChild(li);
                    links.push({ a: a, id: h.id });
                });
                aside.appendChild(ul);

                var helpful = el("div", "lx-helpful", "Was this helpful?");
                var row = el("div", "lx-helpful-row");
                [["👍", "Yes"], ["👎", "No"]].forEach(function (pair) {
                    var b = el("button", null, pair[0]);
                    b.type = "button";
                    b.setAttribute("aria-label", pair[1]);
                    b.addEventListener("click", function () {
                        helpful.textContent = "Thanks for the feedback!";
                    });
                    row.appendChild(b);
                });
                helpful.appendChild(row);
                aside.appendChild(helpful);

                content.appendChild(aside);

                if ("IntersectionObserver" in window) {
                    var byId = {};
                    links.forEach(function (l) { byId[l.id] = l.a; });
                    var io = new IntersectionObserver(function (entries) {
                        entries.forEach(function (en) {
                            if (!en.isIntersecting) return;
                            links.forEach(function (l) { l.a.classList.remove("active"); });
                            var a = byId[en.target.id];
                            if (a) a.classList.add("active");
                        });
                    }, { rootMargin: "0px 0px -75% 0px", threshold: 0 });
                    heads.forEach(function (h) { io.observe(h); });
                }
            }
        }

        // ---- 2. Footer -------------------------------------------------
        if (!document.querySelector(".lx-footer")) {
            var repo = "https://github.com/pilotspace/lunaris";
            var cols = [
                ["Project", [
                    ["Why Lunaris", root + "getting-started/why-lunaris.html"],
                    ["Architecture", root + "getting-started/architecture.html"],
                    ["Quickstart", root + "getting-started/quickstart.html"]
                ]],
                ["Guides", [
                    ["Retrieval DSL", root + "guides/retrieval-dsl.html"],
                    ["MCP Server", root + "mcp/index.html"],
                    ["Cookbook", root + "cookbook/index.html"]
                ]],
                ["Resources", [
                    ["GitHub", repo],
                    ["Issues", repo + "/issues"],
                    ["Configuration", root + "reference/configuration.html"]
                ]]
            ];

            var footer = el("footer", "lx-footer");
            var grid = el("div", "lx-footer-cols");
            cols.forEach(function (c) {
                var col = el("div", "lx-footer-col");
                col.appendChild(el("h4", null, c[0]));
                c[1].forEach(function (lnk) {
                    var a = el("a", null, lnk[0]);
                    a.href = lnk[1];
                    if (/^https?:/.test(lnk[1])) {
                        a.target = "_blank";
                        a.rel = "noopener";
                    }
                    col.appendChild(a);
                });
                grid.appendChild(col);
            });
            footer.appendChild(grid);

            var bar = el("div", "lx-footer-bar");
            bar.appendChild(el("span", "lx-footer-brand", "Lunaris"));
            bar.appendChild(el("span", null, "Apache-2.0 / MIT · Sub-25 ms agent memory in Rust"));
            footer.appendChild(bar);

            content.appendChild(footer);
        }
    });
})();
