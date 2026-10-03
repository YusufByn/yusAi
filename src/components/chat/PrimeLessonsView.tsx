import { useCallback, useEffect, useState } from "react";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import type {
  LessonEventView,
  LessonKind,
  LessonLevel,
  LessonView,
  LessonsOverview,
  ProposalView,
  RefineDetail,
  RefineView,
} from "../../types";

// The "Lessons" view of the Prime chat (prime_review.rs): pending proposals
// of every project (Review), the project's lessons (Lessons) and the
// refines imported for it, refine by refine (Refines).

type Tab = "review" | "lessons" | "refines";

type Props = {
  workspacePath: string;
  // Bumped by the pane to re-read (after Remember, for instance).
  refreshKey: number;
};

export function PrimeLessonsView({ workspacePath, refreshKey }: Props) {
  const [tab, setTab] = useState<Tab>("review");
  const [overview, setOverview] = useState<LessonsOverview | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setOverview(await api.primeLessonsOverview(workspacePath));
      setError(null);
    } catch (err) {
      setError(String(err));
    }
  }, [workspacePath]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  const tabs: { id: Tab; label: string; count?: number }[] = [
    { id: "review", label: "Review", count: overview?.proposals.length },
    { id: "lessons", label: "Lessons" },
    { id: "refines", label: "Refines" },
  ];

  return (
    <div className="chat-body prime-lessons">
      <div className="prime-lessons__tabs" role="tablist">
        {tabs.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={tab === entry.id}
            className="prime-lessons__tab"
            data-active={tab === entry.id ? "true" : "false"}
            onClick={() => setTab(entry.id)}
          >
            {entry.label}
            {entry.count ? <span className="prime-lessons__count">{entry.count}</span> : null}
          </button>
        ))}
      </div>
      <div className="prime-lessons__content">
        {error && <div className="prime-chat__error">{error}</div>}
        {!overview && !error && <div className="prime-chat__status">Loading…</div>}
        {overview && tab === "review" && <ReviewTab overview={overview} />}
        {overview && tab === "lessons" && <LessonsTab overview={overview} />}
        {overview && tab === "refines" && <RefinesTab overview={overview} />}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- Review

function ReviewTab({ overview }: { overview: LessonsOverview }) {
  if (overview.proposals.length === 0) {
    return <Empty text="Nothing waits for your review." />;
  }
  return (
    <div className="prime-lessons__list">
      {overview.proposals.map((proposal) => (
        <ProposalCard key={proposal.id} proposal={proposal} />
      ))}
    </div>
  );
}

function ProposalCard({ proposal }: { proposal: ProposalView }) {
  const lesson = proposal.lesson;
  const payload = proposal.payload;
  return (
    <div className="prime-lessons__card">
      <div className="prime-lessons__card-head">
        <span className="prime-lessons__badge" data-kind={proposal.kind}>
          {proposalLabel(proposal)}
        </span>
        <span className="prime-lessons__meta">
          {[projectName(proposal.project), proposal.conversationTitle, formatDate(proposal.createdAtMs)]
            .filter(Boolean)
            .join(" · ")}
        </span>
      </div>
      {proposal.kind === "change" && lesson ? (
        <div className="prime-lessons__diff">
          <LessonText title={lesson.title} content={lesson.content} muted />
          <Icon icon="solar:arrow-down-linear" width={12} height={12} />
          <LessonText
            title={stringField(payload, "title") ?? lesson.title}
            content={stringField(payload, "content") ?? ""}
          />
        </div>
      ) : proposal.kind === "skill" ? (
        <SkillText payload={payload} />
      ) : lesson ? (
        <LessonText title={lesson.title} content={lesson.content} level={lesson.level} />
      ) : (
        <div className="prime-chat__status">The lesson no longer exists.</div>
      )}
    </div>
  );
}

function SkillText({ payload }: { payload: Record<string, unknown> }) {
  const entry = (payload.entry ?? {}) as Record<string, unknown>;
  const reference = (payload.reference ?? entry.reference ?? {}) as Record<string, unknown>;
  const target = [reference.import, reference.callable].filter(Boolean).join(".");
  return (
    <div className="prime-lessons__text">
      <div className="prime-lessons__title">
        {stringField(entry, "title") ?? stringField(payload, "id") ?? "Skill"}
      </div>
      <div className="prime-lessons__body">{stringField(entry, "content") ?? ""}</div>
      {target && <code className="prime-lessons__code">{target}</code>}
    </div>
  );
}

// ---------------------------------------------------------------- Lessons

function LessonsTab({ overview }: { overview: LessonsOverview }) {
  const [showArchived, setShowArchived] = useState(false);
  const lessons = overview.lessons.filter(
    (lesson) => showArchived || lesson.status === "active",
  );
  const groups: { level: LessonLevel; label: string }[] = [
    { level: "project", label: "Project" },
    {
      level: "type",
      label: overview.projectType ? `Type: ${overview.projectType}` : "Type",
    },
    { level: "global", label: "Global" },
  ];
  return (
    <div className="prime-lessons__list">
      <label className="prime-lessons__toggle">
        <input
          type="checkbox"
          checked={showArchived}
          onChange={(event) => setShowArchived(event.target.checked)}
        />
        Show archived
      </label>
      {lessons.length === 0 && <Empty text="No lessons for this project yet." />}
      {groups.map(({ level, label }) => {
        const inGroup = lessons.filter((lesson) => lesson.level === level);
        if (inGroup.length === 0) return null;
        return (
          <section key={level} className="prime-lessons__group">
            <h3 className="prime-lessons__group-title">{label}</h3>
            {inGroup.map((lesson) => (
              <LessonRow key={lesson.id} lesson={lesson} />
            ))}
          </section>
        );
      })}
    </div>
  );
}

function LessonRow({ lesson }: { lesson: LessonView }) {
  const [open, setOpen] = useState(false);
  const [history, setHistory] = useState<LessonEventView[] | null>(null);
  useEffect(() => {
    if (!open || history) return;
    api.primeLessonHistory(lesson.id).then(setHistory).catch(console.error);
  }, [open, history, lesson.id]);
  return (
    <div className="prime-lessons__card" data-archived={lesson.status === "archived"}>
      <button
        type="button"
        className="prime-lessons__row-head"
        onClick={() => setOpen((now) => !now)}
        aria-expanded={open}
      >
        <span className="prime-lessons__badge" data-kind={lesson.kind}>
          {kindLabel(lesson.kind)}
        </span>
        <span className="prime-lessons__title">{lesson.title}</span>
        {lesson.pinned && <span className="prime-lessons__flag">Pinned</span>}
        {lesson.status === "archived" && <span className="prime-lessons__flag">Archived</span>}
        {lesson.status === "active" && !lesson.injected && (
          <span className="prime-lessons__flag" title="Beyond the 4,000-character budget">
            Not injected
          </span>
        )}
        <Icon
          icon={open ? "solar:alt-arrow-up-linear" : "solar:alt-arrow-down-linear"}
          width={11}
          height={11}
          className="prime-lessons__chevron"
        />
      </button>
      <div className="prime-lessons__body">{lesson.content}</div>
      {open && (
        <div className="prime-lessons__history">
          {!history && <div className="prime-chat__status">Loading…</div>}
          {history?.map((event) => (
            <EventLine key={event.id} event={event} />
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- Refines

function RefinesTab({ overview }: { overview: LessonsOverview }) {
  if (overview.refines.length === 0) {
    return <Empty text="No refine imported for this project yet." />;
  }
  return (
    <div className="prime-lessons__list">
      {overview.refines.map((refine) => (
        <RefineRow key={refine.refinementId} refine={refine} />
      ))}
    </div>
  );
}

function RefineRow({ refine }: { refine: RefineView }) {
  const [open, setOpen] = useState(false);
  const [detail, setDetail] = useState<RefineDetail | null>(null);
  useEffect(() => {
    if (!open || detail) return;
    api.primeRefineDetail(refine.refinementId).then(setDetail).catch(console.error);
  }, [open, detail, refine.refinementId]);
  const counts = [
    refine.created && `${refine.created} created`,
    refine.updated && `${refine.updated} updated`,
    refine.archived && `${refine.archived} archived`,
    refine.duplicates && `${refine.duplicates} duplicate${refine.duplicates === 1 ? "" : "s"}`,
    refine.proposals && `${refine.proposals} proposal${refine.proposals === 1 ? "" : "s"}`,
    refine.skipped.length && `${refine.skipped.length} skipped`,
    refine.failures.length && `${refine.failures.length} failed`,
  ].filter(Boolean);
  return (
    <div className="prime-lessons__card" data-archived={refine.undoneAtMs !== null}>
      <button
        type="button"
        className="prime-lessons__row-head"
        onClick={() => setOpen((now) => !now)}
        aria-expanded={open}
      >
        <span className="prime-lessons__badge">{actorLabel(refine.actor)}</span>
        <span className="prime-lessons__meta">
          {[formatDate(refine.importedAtMs), refine.conversationTitle].filter(Boolean).join(" · ")}
        </span>
        {refine.undoneAtMs !== null && <span className="prime-lessons__flag">Undone</span>}
        <Icon
          icon={open ? "solar:alt-arrow-up-linear" : "solar:alt-arrow-down-linear"}
          width={11}
          height={11}
          className="prime-lessons__chevron"
        />
      </button>
      {refine.summary && <div className="prime-lessons__title">{refine.summary}</div>}
      <div className="prime-lessons__meta">{counts.length ? counts.join(" · ") : "Nothing kept"}</div>
      {open && (
        <div className="prime-lessons__history">
          {!detail && <div className="prime-chat__status">Loading…</div>}
          {detail?.events.map((event) => (
            <EventLine key={event.id} event={event} withLesson />
          ))}
          {detail?.proposals.map((proposal) => (
            <div key={proposal.id} className="prime-lessons__event">
              <span className="prime-lessons__event-action">
                {proposal.kind === "promote"
                  ? `Proposed: move to ${levelLabel(proposal.targetLevel)}`
                  : `Proposed: ${proposal.kind}`}
              </span>
              <span className="prime-lessons__meta">{proposal.status}</span>
            </div>
          ))}
          {refine.skipped.map((reason) => (
            <div key={reason} className="prime-lessons__event">
              <span className="prime-lessons__event-action">Skipped</span>
              <span className="prime-lessons__meta">{reason}</span>
            </div>
          ))}
          {refine.failures.map((reason) => (
            <div key={reason} className="prime-lessons__event" data-failed="true">
              <span className="prime-lessons__event-action">Failed</span>
              <span className="prime-lessons__meta">{reason}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- Shared

function EventLine({ event, withLesson }: { event: LessonEventView; withLesson?: boolean }) {
  const before = event.before;
  const after = event.after;
  const textChanged =
    before && after && (before.content !== after.content || before.title !== after.title);
  const lesson = after ?? event.lesson;
  return (
    <div className="prime-lessons__event">
      <span className="prime-lessons__event-action">{eventLabel(event)}</span>
      <span className="prime-lessons__meta">
        {[actorLabel(event.actor), formatDate(event.atMs), event.conversationTitle]
          .filter(Boolean)
          .join(" · ")}
      </span>
      {withLesson && lesson && !textChanged && (
        <LessonText title={lesson.title} content={lesson.content} />
      )}
      {textChanged && before && after && (
        <div className="prime-lessons__diff">
          <LessonText title={before.title} content={before.content} muted />
          <Icon icon="solar:arrow-down-linear" width={12} height={12} />
          <LessonText title={after.title} content={after.content} />
        </div>
      )}
    </div>
  );
}

function LessonText({
  title,
  content,
  level,
  muted,
}: {
  title: string;
  content: string;
  level?: LessonLevel;
  muted?: boolean;
}) {
  return (
    <div className="prime-lessons__text" data-muted={muted ? "true" : "false"}>
      <div className="prime-lessons__title">
        {title}
        {level && <span className="prime-lessons__meta"> · {levelLabel(level)}</span>}
      </div>
      <div className="prime-lessons__body">{content}</div>
    </div>
  );
}

function Empty({ text }: { text: string }) {
  return <div className="prime-chat__status">{text}</div>;
}

function actorLabel(actor: string | null): string {
  switch (actor) {
    case "refine:retain":
      return "Remember";
    case "refine:close":
      return "Closing";
    case "refine:global":
      return "Prime (global)";
    case "harness:global":
      return "Global harness";
    case "user":
      return "You";
    case "refine":
    case null:
      return "Prime";
    default:
      return actor;
  }
}

function eventLabel(event: LessonEventView): string {
  switch (event.action) {
    case "created":
      return "Created";
    case "updated":
      return "Updated";
    case "archived":
      return "Archived";
    case "restored":
      return "Restored";
    case "promoted":
      return `Moved to ${levelLabel(event.after?.level ?? null)}`;
    case "demoted":
      return `Moved down to ${levelLabel(event.after?.level ?? null)}`;
    case "pinned":
      return "Pinned";
    case "unpinned":
      return "Unpinned";
    case "duplicate_skipped":
      return "Duplicate skipped";
    case "rejected":
      return "Proposal rejected";
    default:
      return event.action;
  }
}

function proposalLabel(proposal: ProposalView): string {
  switch (proposal.kind) {
    case "promote":
      return `Move to ${levelLabel(proposal.targetLevel)}`;
    case "change":
      return `Change (${levelLabel(proposal.lesson?.level ?? proposal.targetLevel)})`;
    case "archive":
      return `Archive (${levelLabel(proposal.lesson?.level ?? proposal.targetLevel)})`;
    case "skill":
      return "Skill";
  }
}

function levelLabel(level: LessonLevel | null): string {
  switch (level) {
    case "project":
      return "project";
    case "type":
      return "type";
    case "global":
      return "global";
    default:
      return "?";
  }
}

function kindLabel(kind: LessonKind): string {
  switch (kind) {
    case "memory":
      return "Fact";
    case "prompt":
      return "Instruction";
    case "subagent":
      return "Role";
  }
}

function projectName(project: string | null): string | null {
  if (!project) return null;
  return project.split(/[\\/]/).filter(Boolean).pop() ?? project;
}

function stringField(record: Record<string, unknown>, key: string): string | null {
  const value = record[key];
  return typeof value === "string" && value.trim() ? value : null;
}

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
