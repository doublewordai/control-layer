// Applies the saved theme before first paint. next-themes takes over once React mounts.
(function () {
  try {
    var t = localStorage.getItem("theme");
    var dark =
      t === "dark" ||
      ((!t || t === "system") &&
        window.matchMedia("(prefers-color-scheme: dark)").matches);
    if (dark) document.documentElement.classList.add("dark");
  } catch (e) {}
})();
