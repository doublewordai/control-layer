import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const sidebarScriptUrl = new URL("../../docs/sidebar-links.js", import.meta.url);

class FakeElement {
  constructor(tagName) {
    this.tagName = tagName.toUpperCase();
    this.children = [];
    this.attributes = new Map();
    this.className = "";
    this.href = "";
    this.rel = "";
    this.target = "";
    this.textContent = "";
  }

  append(...children) {
    this.children.push(...children);
  }

  appendChild(child) {
    this.children.push(child);
    return child;
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, value);
  }
}

async function loadSidebarScript() {
  try {
    return await readFile(sidebarScriptUrl, "utf8");
  } catch (error) {
    assert.fail(`Unable to load the sidebar extension: ${error.message}`);
  }
}

async function runSidebarScript(chapterList) {
  const source = await loadSidebarScript();
  const document = {
    createElement(tagName) {
      return new FakeElement(tagName);
    },
    querySelector(selector) {
      assert.equal(selector, "#mdbook-sidebar .chapter");
      return chapterList;
    },
  };

  vm.runInNewContext(source, { document });
}

test("appends an accessible Onwards link after a divider", async () => {
  const chapterList = new FakeElement("ol");

  await runSidebarScript(chapterList);

  assert.equal(chapterList.children.length, 2);

  const [divider, item] = chapterList.children;
  assert.equal(divider.tagName, "LI");
  assert.equal(divider.className, "spacer");
  assert.equal(divider.getAttribute("aria-hidden"), "true");

  assert.equal(item.tagName, "LI");
  assert.equal(item.className, "chapter-item");

  const [wrapper] = item.children;
  assert.equal(wrapper.tagName, "SPAN");
  assert.equal(wrapper.className, "chapter-link-wrapper");

  const [link] = wrapper.children;
  assert.equal(link.tagName, "A");
  assert.equal(
    link.href,
    "https://doublewordai.github.io/control-layer/onwards/",
  );
  assert.equal(link.target, "_blank");
  assert.equal(link.rel, "noopener noreferrer");
  assert.equal(link.textContent, "Onwards ↗");
  assert.equal(
    link.getAttribute("aria-label"),
    "Onwards documentation (opens in a new tab)",
  );
});

test("does nothing when the sidebar chapter list is absent", async () => {
  await runSidebarScript(null);
});
