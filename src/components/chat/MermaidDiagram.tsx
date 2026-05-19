import { Icon } from "@iconify/react";
import {
  memo,
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { loadTheme, subscribeTheme, type Theme } from "../../lib/theme";

type MermaidApi = typeof import("mermaid").default;

type MermaidRenderState =
  | { status: "loading" }
  | { status: "ready"; svg: string }
  | { status: "error"; message: string };

let mermaidPromise: Promise<MermaidApi> | null = null;
// Tracks which palette mermaid was last initialised with. `null` means
// it has never been initialised. When the user toggles the theme we
// re-run `mermaid.initialize` so subsequent renders pick up the new
// palette, then signal subscribed diagrams to re-render their SVG.
let mermaidInitializedTheme: Theme | null = null;
let mermaidIdCounter = 0;

function loadMermaid(): Promise<MermaidApi> {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then((module) => module.default);
  }
  return mermaidPromise;
}

/** Mermaid theme variables for both palettes. Kept in one place so the
 *  two stay obviously in sync — bump a key in both at once. */
const MERMAID_THEMES: Record<Theme, Record<string, string>> = {
  dark: {
    background: "#08090b",
    mainBkg: "#141518",
    primaryColor: "#141518",
    primaryBorderColor: "#3a3d44",
    primaryTextColor: "#e8e9ec",
    secondaryColor: "#0f1013",
    tertiaryColor: "#181a1f",
    lineColor: "#6b6f78",
    textColor: "#e8e9ec",
    titleColor: "#e8e9ec",
    clusterBkg: "#0f1013",
    clusterBorder: "#23252b",
    edgeLabelBackground: "#0b0b0d",
    nodeBorder: "#3a3d44",
    actorBkg: "#141518",
    actorBorder: "#3a3d44",
    actorTextColor: "#e8e9ec",
    actorLineColor: "#6b6f78",
    signalColor: "#d2d4d9",
    signalTextColor: "#d2d4d9",
    labelBoxBkgColor: "#141518",
    labelBoxBorderColor: "#3a3d44",
    labelTextColor: "#e8e9ec",
    loopTextColor: "#e8e9ec",
    noteBkgColor: "#181a1f",
    noteBorderColor: "#3a3d44",
    noteTextColor: "#e8e9ec",
    activationBkgColor: "#1e2025",
    activationBorderColor: "#3a3d44",
    sectionBkgColor: "#141518",
    altSectionBkgColor: "#0f1013",
    gridColor: "#23252b",
    taskBkgColor: "#141518",
    taskTextColor: "#e8e9ec",
    taskTextLightColor: "#e8e9ec",
    taskTextOutsideColor: "#d2d4d9",
    taskTextClickableColor: "#9fc2ff",
    activeTaskBkgColor: "#1e2b4a",
    activeTaskBorderColor: "#3b82f6",
    doneTaskBkgColor: "#14311f",
    doneTaskBorderColor: "#22c55e",
    critBkgColor: "#3a1d22",
    critBorderColor: "#f5737f",
    todayLineColor: "#f5a683",
  },
  light: {
    background: "#ffffff",
    mainBkg: "#ffffff",
    primaryColor: "#ffffff",
    primaryBorderColor: "#c5cad1",
    primaryTextColor: "#1a1c1f",
    secondaryColor: "#f4f5f7",
    tertiaryColor: "#eef0f3",
    lineColor: "#8a929c",
    textColor: "#1a1c1f",
    titleColor: "#1a1c1f",
    clusterBkg: "#f7f8fa",
    clusterBorder: "#dde1e7",
    edgeLabelBackground: "#ffffff",
    nodeBorder: "#c5cad1",
    actorBkg: "#ffffff",
    actorBorder: "#c5cad1",
    actorTextColor: "#1a1c1f",
    actorLineColor: "#8a929c",
    signalColor: "#2c3038",
    signalTextColor: "#2c3038",
    labelBoxBkgColor: "#f4f5f7",
    labelBoxBorderColor: "#c5cad1",
    labelTextColor: "#1a1c1f",
    loopTextColor: "#1a1c1f",
    noteBkgColor: "#fef9c3",
    noteBorderColor: "#eab308",
    noteTextColor: "#1a1c1f",
    activationBkgColor: "#e6e9ee",
    activationBorderColor: "#c5cad1",
    sectionBkgColor: "#f4f5f7",
    altSectionBkgColor: "#ffffff",
    gridColor: "#dde1e7",
    taskBkgColor: "#f4f5f7",
    taskTextColor: "#1a1c1f",
    taskTextLightColor: "#1a1c1f",
    taskTextOutsideColor: "#2c3038",
    taskTextClickableColor: "#1d4ed8",
    activeTaskBkgColor: "#dbeafe",
    activeTaskBorderColor: "#2563eb",
    doneTaskBkgColor: "#dcfce7",
    doneTaskBorderColor: "#16a34a",
    critBkgColor: "#fee2e2",
    critBorderColor: "#dc2626",
    todayLineColor: "#c2410c",
  },
};

/**
 * Custom event mermaid diagrams listen to after a theme switch — tells
 * mounted instances "your SVG was rendered with the old palette, please
 * re-run the render effect now". Kept module-local so it isn't visible
 * outside this file.
 */
const MERMAID_REPAINT_EVENT = "sinew:mermaid-repaint";

/**
 * Wire mermaid re-initialisation to global theme changes. Runs once per
 * page lifetime (subscribeTheme is idempotent and the listener cleans
 * itself up implicitly when the page unloads).
 */
subscribeTheme(() => {
  // Force re-init on the next renderMermaid call.
  mermaidInitializedTheme = null;
  try {
    window.dispatchEvent(new Event(MERMAID_REPAINT_EVENT));
  } catch {
    /* no-op in non-browser test envs */
  }
});

async function renderMermaid(source: string, id: string, theme: Theme) {
  const mermaid = await loadMermaid();
  if (mermaidInitializedTheme !== theme) {
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      theme: "base",
      darkMode: theme === "dark",
      fontFamily: '"Geist", ui-sans-serif, system-ui, sans-serif',
      themeVariables: MERMAID_THEMES[theme],
    });
    mermaidInitializedTheme = theme;
  }
  return mermaid.render(id, source);
}

function formatMermaidError(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

type Props = {
  source: string;
};

/**
 * Renders a Mermaid diagram block. The compiled SVG is clickable: it
 * pops open a fullscreen lightbox with mouse-wheel zoom and click-drag
 * pan so users can inspect dense diagrams comfortably.
 */
export const MermaidDiagram = memo(function MermaidDiagram({ source }: Props) {
  const [state, setState] = useState<MermaidRenderState>({ status: "loading" });
  const [zoomed, setZoomed] = useState(false);
  // Bumping this counter triggers a re-render of the diagram after a
  // global theme switch, since mermaid produced its SVG against the
  // previous palette. The counter is local to each diagram instance.
  const [themeRepaint, setThemeRepaint] = useState(0);
  const instanceIdRef = useRef<string | null>(null);
  const renderSeqRef = useRef(0);

  if (!instanceIdRef.current) {
    mermaidIdCounter += 1;
    instanceIdRef.current = `sinew-mermaid-${mermaidIdCounter}`;
  }

  useEffect(() => {
    const onRepaint = () => setThemeRepaint((n) => n + 1);
    window.addEventListener(MERMAID_REPAINT_EVENT, onRepaint);
    return () => window.removeEventListener(MERMAID_REPAINT_EVENT, onRepaint);
  }, []);

  useEffect(() => {
    const normalizedSource = source.replace(/\n$/, "");
    if (!normalizedSource.trim()) {
      setState({ status: "error", message: "Diagramme Mermaid vide." });
      return;
    }

    let cancelled = false;
    const renderId = `${instanceIdRef.current}-${++renderSeqRef.current}`;
    setState({ status: "loading" });

    void renderMermaid(normalizedSource, renderId, loadTheme())
      .then((result) => {
        if (cancelled) return;
        setState({ status: "ready", svg: result.svg });
      })
      .catch((error) => {
        if (cancelled) return;
        setState({ status: "error", message: formatMermaidError(error) });
      });

    return () => {
      cancelled = true;
    };
  }, [source, themeRepaint]);

  const openZoom = useCallback(() => setZoomed(true), []);
  const closeZoom = useCallback(() => setZoomed(false), []);

  if (state.status === "ready") {
    return (
      <>
        <div
          className="mermaid-diagram"
          data-state="ready"
          role="button"
          tabIndex={0}
          title="Click to zoom"
          onClick={openZoom}
          onKeyDown={(event) => {
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              openZoom();
            }
          }}
        >
          <div
            className="mermaid-diagram__svg"
            dangerouslySetInnerHTML={{ __html: state.svg }}
          />
        </div>
        {zoomed && <MermaidLightbox svg={state.svg} onClose={closeZoom} />}
      </>
    );
  }

  if (state.status === "error") {
    return (
      <div className="mermaid-diagram" data-state="error">
        <div className="mermaid-diagram__error-title">Mermaid render error</div>
        <pre className="mermaid-diagram__error-message">{state.message}</pre>
      </div>
    );
  }

  return (
    <div className="mermaid-diagram" data-state="loading">
      <span className="mermaid-diagram__loading">Rendering Mermaid diagram…</span>
    </div>
  );
});

type LightboxProps = {
  svg: string;
  onClose: () => void;
};

const MIN_SCALE = 0.25;
const MAX_SCALE = 8;

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function MermaidLightbox({ svg, onClose }: LightboxProps) {
  const [scale, setScale] = useState(1);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const [grabbing, setGrabbing] = useState(false);
  const dragRef = useRef<{
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);

  // Close on Escape.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // React's onWheel handler is passive in modern React; we can't call
  // preventDefault from it. Attach a non-passive native listener so the
  // page never scrolls behind the modal while zooming.
  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const onWheel = (event: WheelEvent) => {
      event.preventDefault();
      const factor = event.deltaY < 0 ? 1.15 : 1 / 1.15;
      setScale((prev) => clamp(prev * factor, MIN_SCALE, MAX_SCALE));
    };
    stage.addEventListener("wheel", onWheel, { passive: false });
    return () => stage.removeEventListener("wheel", onWheel);
  }, []);

  const onMouseDown = (event: ReactMouseEvent<HTMLDivElement>) => {
    dragRef.current = {
      startX: event.clientX,
      startY: event.clientY,
      originX: offset.x,
      originY: offset.y,
    };
    setGrabbing(true);
  };

  const onMouseMove = (event: ReactMouseEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag) return;
    setOffset({
      x: drag.originX + (event.clientX - drag.startX),
      y: drag.originY + (event.clientY - drag.startY),
    });
  };

  const endDrag = () => {
    dragRef.current = null;
    setGrabbing(false);
  };

  const reset = () => {
    setScale(1);
    setOffset({ x: 0, y: 0 });
  };

  const zoomIn = () =>
    setScale((prev) => clamp(prev * 1.2, MIN_SCALE, MAX_SCALE));
  const zoomOut = () =>
    setScale((prev) => clamp(prev / 1.2, MIN_SCALE, MAX_SCALE));

  const stop = (event: ReactMouseEvent) => event.stopPropagation();

  return (
    <div
      className="mermaid-zoom"
      role="dialog"
      aria-modal="true"
      aria-label="Mermaid diagram preview"
      onClick={onClose}
    >
      <div className="mermaid-zoom__toolbar" onClick={stop}>
        <button type="button" onClick={zoomOut} title="Zoom out">
          <Icon icon="solar:minus-circle-linear" width={16} height={16} />
        </button>
        <span className="mermaid-zoom__scale">{Math.round(scale * 100)}%</span>
        <button type="button" onClick={zoomIn} title="Zoom in">
          <Icon icon="solar:add-circle-linear" width={16} height={16} />
        </button>
        <button type="button" onClick={reset} title="Reset view">
          <Icon icon="solar:refresh-linear" width={16} height={16} />
        </button>
        <span className="mermaid-zoom__sep" aria-hidden />
        <button type="button" onClick={onClose} title="Close (Esc)">
          <Icon icon="solar:close-circle-linear" width={16} height={16} />
        </button>
      </div>
      <div
        ref={stageRef}
        className="mermaid-zoom__stage"
        data-grabbing={grabbing ? "true" : "false"}
        onClick={stop}
        onMouseDown={onMouseDown}
        onMouseMove={onMouseMove}
        onMouseUp={endDrag}
        onMouseLeave={endDrag}
      >
        <div
          className="mermaid-zoom__svg"
          style={{
            transform: `translate(${offset.x}px, ${offset.y}px) scale(${scale})`,
          }}
          dangerouslySetInnerHTML={{ __html: svg }}
        />
      </div>
      <div className="mermaid-zoom__hint" onClick={stop}>
        Scroll to zoom · drag to pan · Esc to close
      </div>
    </div>
  );
}
