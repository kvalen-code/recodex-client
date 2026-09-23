import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { readFile } from "node:fs/promises";

type FakeElementOptions = {
  className?: string;
  closestMatch?: string;
  dismissLabel?: string;
  hasProgress?: boolean;
  hasUpgradeAction?: boolean;
  headingText?: string;
  styleDisplay?: string;
};

class FakeElement {
  children: FakeElement[] = [];
  dataset: Record<string, string> = {};
  parentElement: FakeElement | null = null;
  style: { display: string };
  private readonly className: string;
  private readonly closestMatch?: string;
  private readonly dismissLabel: string;
  private readonly hasProgress: boolean;
  private readonly hasUpgradeAction: boolean;
  private readonly headingText?: string;

  constructor(options: FakeElementOptions = {}) {
    this.className = options.className ?? "";
    this.closestMatch = options.closestMatch;
    this.dismissLabel = options.dismissLabel ?? "";
    this.hasProgress = options.hasProgress ?? false;
    this.hasUpgradeAction = options.hasUpgradeAction ?? false;
    this.headingText = options.headingText;
    this.style = { display: options.styleDisplay ?? "" };
  }

  appendChild(child: FakeElement) {
    child.parentElement = this;
    this.children.push(child);
  }

  closest(selector: string) {
    return this.closestMatch === selector ? this : null;
  }

  getAttribute(name: string) {
    return name === "aria-label" ? this.dismissLabel : null;
  }

  matches(selector: string) {
    return selector === "div.w-full" && this.className.split(/\s+/).includes("w-full");
  }

  querySelector(selector: string) {
    if (selector === 'progress[max="100"]') {
      return this.hasProgress ? new FakeElement() : null;
    }
    if (/heading|h[1-5]/.test(selector) && this.headingText) {
      return { textContent: this.headingText };
    }
    if (/billing|upgrade/i.test(selector) && this.hasUpgradeAction) {
      return new FakeElement();
    }
    return null;
  }

  querySelectorAll(selector: string) {
    return selector === "button" && this.dismissLabel ? [this] : [];
  }
}

function usageAlertRuntime(
  renderer: string,
  cards: FakeElement[],
  managed: FakeElement[],
  composerBanners: FakeElement[] = [],
) {
  const start = renderer.indexOf("  function officialUsageAlertHidden(");
  const end = renderer.indexOf("\n  let zedRemoteStatusPromise", start);
  assert.ok(start >= 0 && end > start);
  const source = renderer.slice(start, end);
  const selectors: string[] = [];
  const bodyClasses = new Set<string>();
  const document = {
    body: {
      classList: {
        contains(cls: string) {
          return bodyClasses.has(cls);
        },
        toggle(cls: string, force?: boolean) {
          const next = force === undefined ? !bodyClasses.has(cls) : !!force;
          if (next) {
            bodyClasses.add(cls);
          } else {
            bodyClasses.delete(cls);
          }
          return next;
        },
      },
    },
    querySelectorAll(selector: string) {
      selectors.push(selector);
      if (selector === '[data-codex-plus-usage-alert-hidden="true"]') {
        return managed.filter((node) => node.dataset.codexPlusUsageAlertHidden === "true");
      }
      if (selector === '[data-codex-plus-usage-alert-hidden]') {
        return [...managed, ...cards, ...composerBanners].filter((node) => "codexPlusUsageAlertHidden" in node.dataset);
      }
      if (selector === '[data-codex-composer-root] aside') {
        return composerBanners;
      }
      return cards;
    },
  };
  const windowValue: Record<string, unknown> = {};
  const create = new Function(
    "window",
    "document",
    "HTMLElement",
    `${source}\nreturn { officialUsageAlertHidden, refreshOfficialUsageAlertVisibility };`,
  ) as (
    windowValue: Record<string, unknown>,
    documentValue: typeof document,
    elementType: typeof FakeElement,
  ) => {
    officialUsageAlertHidden: () => boolean;
    refreshOfficialUsageAlertVisibility: () => void;
  };
  return { runtime: create(windowValue, document, FakeElement), selectors, windowValue, bodyClasses };
}

function installRendererStyle(renderer: string) {
  const start = renderer.indexOf("  function installStyle()");
  const end = renderer.indexOf("\n  function defaultCodexPlusSettings", start);
  assert.ok(start >= 0 && end > start);
  const source = renderer.slice(start, end);
  const requiredNames = new Set([
    "styleId",
    "codexDeleteStyleVersion",
    ...Array.from(source.matchAll(/\$\{([A-Za-z_$][A-Za-z0-9_$]*)/g), (match) => match[1]),
  ]);
  const declarations = Array.from(requiredNames, (name) => {
    const declaration = renderer.match(new RegExp(`^  const ${name} = .+;$`, "m"))
      ?? renderer.match(new RegExp(`^  const ${name} = [\\s\\S]*?^  };$`, "m"));
    assert.ok(declaration, `missing renderer declaration for ${name}`);
    return declaration[0];
  }).join("\n");
  const appended: Array<{ dataset: Record<string, string>; id?: string; textContent?: string }> = [];
  const document = {
    getElementById() {
      return null;
    },
    createElement() {
      return { dataset: {} };
    },
    documentElement: {
      appendChild(node: (typeof appended)[number]) {
        appended.push(node);
      },
    },
  };
  const install = new Function("document", `${declarations}\n${source}\ninstallStyle();`) as (documentValue: typeof document) => void;

  install(document);
  return appended;
}

describe("renderer injection header compatibility", () => {
  it("anchors the Codex++ menu to current and legacy application top bars only", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");

    assert.match(renderer, /appHeader:\s*'[^"]*\[class\*="ApplicationMenuTopBar"\][^']*\.app-header-tint'/);
    assert.doesNotMatch(renderer, /document\.querySelector\(["']header["']\)/);
    assert.match(renderer, /isApplicationMenuTopBar\s*\?\s*Math\.max\(4, headerRect\.top\)/);
    assert.match(renderer, /isApplicationMenuTopBar\s*\?\s*28\s*:\s*headerRect\.height/);
  });

  it("does not install Codex++ UI in embedded browser documents", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");

    assert.match(renderer, /window\.top\s*!==\s*window/);
    assert.match(renderer, /!window\.electronBridge/);
    assert.ok(renderer.includes("/^app:\\\/\\\/\\-\\//i.test(window.location.href)"));
    assert.match(renderer, /codexPlusIsNodeTestHarness/);
  });

  it("initializes renderer styles without unresolved template identifiers", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");

    const appended = installRendererStyle(renderer);

    assert.equal(appended.length, 1);
    assert.match(appended[0].textContent ?? "", /#codex-plus-menu/);
  });

  it("hides only the official usage alert and restores it without changing upstream styles", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");
    const wrapper = new FakeElement({ className: "w-full", styleDisplay: "grid" });
    const usageAlert = new FakeElement({ dismissLabel: "Dismiss usage alert", hasProgress: true });
    const otherStatus = new FakeElement({ dismissLabel: "Dismiss sync status", hasProgress: true });
    wrapper.appendChild(usageAlert);
    const { runtime, selectors, windowValue } = usageAlertRuntime(renderer, [usageAlert, otherStatus], [wrapper]);

    windowValue.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = true;
    runtime.refreshOfficialUsageAlertVisibility();

    assert.equal(wrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(wrapper.style.display, "grid");
    assert.equal(otherStatus.dataset.codexPlusUsageAlertHidden, undefined);
    assert.deepEqual(selectors, [
      'aside.app-shell-left-panel [role="status"][aria-live="polite"]',
      '[data-codex-composer-root] aside',
      '[data-codex-plus-usage-alert-hidden]',
    ]);

    windowValue.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = false;
    runtime.refreshOfficialUsageAlertVisibility();

    assert.equal(wrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(wrapper.style.display, "grid");
    assert.equal(wrapper.children[0], usageAlert);
    assert.equal(selectors.pop(), '[data-codex-plus-usage-alert-hidden]');
  });

  it("hides modern composer usage alert banners and restores them without hiding unrelated asides", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");
    const composerWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const matchingBanner = new FakeElement({ headingText: "Codex 和工作使用额度已用完" });
    composerWrapper.appendChild(matchingBanner);

    const englishWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const englishBanner = new FakeElement({ headingText: "You're out of\nCodex and Work usage" });
    englishWrapper.appendChild(englishBanner);

    const workspaceWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const workspaceBanner = new FakeElement({ headingText: "你的 Codex 和工作用量均已用完" });
    workspaceWrapper.appendChild(workspaceBanner);

    const approachingWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const approachingBanner = new FakeElement({ headingText: "您即将达到使用限额" });
    approachingWrapper.appendChild(approachingBanner);

    const modelWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const modelBanner = new FakeElement({ headingText: "所选模型已超出使用限额" });
    modelWrapper.appendChild(modelBanner);

    const unrelatedWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const unrelatedNotice = new FakeElement({ headingText: "Network disconnected" });
    unrelatedWrapper.appendChild(unrelatedNotice);

    const fileErrorWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const fileErrorNotice = new FakeElement({ headingText: "File upload failed" });
    fileErrorWrapper.appendChild(fileErrorNotice);

    const ultraWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const ultraNotice = new FakeElement({
      headingText: "Ultra with up to 5 agents can use your usage limits quickly",
    });
    ultraWrapper.appendChild(ultraNotice);

    const sharedWrapper = new FakeElement({ closestMatch: "[data-codex-composer-root]" });
    const sharedBanner = new FakeElement({ headingText: "此模型的使用额度已用完" });
    const sharedSibling = new FakeElement({ headingText: "Sandbox ready" });
    sharedWrapper.appendChild(sharedBanner);
    sharedWrapper.appendChild(sharedSibling);

    const { runtime, windowValue, bodyClasses } = usageAlertRuntime(
      renderer,
      [],
      [
        composerWrapper,
        englishWrapper,
        workspaceWrapper,
        approachingWrapper,
        modelWrapper,
        unrelatedWrapper,
        fileErrorWrapper,
        ultraWrapper,
        sharedWrapper,
      ],
      [
        matchingBanner,
        englishBanner,
        workspaceBanner,
        approachingBanner,
        modelBanner,
        unrelatedNotice,
        fileErrorNotice,
        ultraNotice,
        sharedBanner,
        sharedSibling,
      ],
    );

    windowValue.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = true;
    runtime.refreshOfficialUsageAlertVisibility();

    assert.equal(composerWrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(englishWrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(workspaceWrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(approachingWrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(modelWrapper.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(unrelatedWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(fileErrorWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(ultraWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(sharedWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(sharedBanner.dataset.codexPlusUsageAlertHidden, "true");
    assert.equal(sharedSibling.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(bodyClasses.has("codex-plus-hide-usage-alert"), true);

    windowValue.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = false;
    runtime.refreshOfficialUsageAlertVisibility();

    assert.equal(composerWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(englishWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(workspaceWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(approachingWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(modelWrapper.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(sharedBanner.dataset.codexPlusUsageAlertHidden, undefined);
    assert.equal(bodyClasses.has("codex-plus-hide-usage-alert"), false);
  });

  it("refreshes active-profile usage alert settings through the existing backend heartbeat", async () => {
    const renderer = await readFile(new URL("../../../assets/inject/renderer-inject.js", import.meta.url), "utf8");

    assert.match(renderer, /typeof nextStatus\.hideOfficialUsageAlert === "boolean"/);
    assert.match(renderer, /window\.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = nextStatus\.hideOfficialUsageAlert/);
    assert.match(renderer, /\[data-codex-plus-usage-alert-hidden="true"\] \{ display: none !important; \}/);
    assert.doesNotMatch(renderer, /container\.style\.(?:setProperty|removeProperty)\("display"/);
  });

  it("keeps Windows Dream Skin compatible with the modern Codex main surface", async () => {
    const windowsRenderers = await Promise.all([
      readFile(new URL("../../../assets/inject/upstream/dream-skin/windows/renderer-inject.js", import.meta.url), "utf8"),
      readFile(new URL("../../../assets/inject/upstream/cidala-tiger/windows/renderer-inject.js", import.meta.url), "utf8"),
    ]);

    for (const renderer of windowsRenderers) {
      assert.match(renderer, /MainContentSurface/);
      assert.match(renderer, /data-codex-plus-dream-surface/);
      assert.match(renderer, /ensureShellMain/);
    }
  });
});
