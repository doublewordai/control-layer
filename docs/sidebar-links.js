(() => {
  const chapterList = document.querySelector("#mdbook-sidebar .chapter");
  if (!chapterList) {
    return;
  }

  const divider = document.createElement("li");
  divider.className = "spacer";
  divider.setAttribute("aria-hidden", "true");

  const item = document.createElement("li");
  item.className = "chapter-item";

  const wrapper = document.createElement("span");
  wrapper.className = "chapter-link-wrapper";

  const link = document.createElement("a");
  link.href = "https://doublewordai.github.io/control-layer/onwards/";
  link.target = "_blank";
  link.rel = "noopener noreferrer";
  link.textContent = "Onwards ↗";
  link.setAttribute(
    "aria-label",
    "Onwards documentation (opens in a new tab)",
  );

  wrapper.appendChild(link);
  item.appendChild(wrapper);
  chapterList.append(divider, item);
})();
