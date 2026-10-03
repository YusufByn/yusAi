import { useCallback, useEffect, useState } from "react";
import { Icon } from "@iconify/react";
import { api } from "../../lib/ipc";
import type {
  ArchivedSkill,
  Lesson,
  LessonEventView,
  LessonKind,
  LessonLevel,
  LessonView,
  LessonsOverview,
  ProposalView,
  RefineDetail,
  RefineView,
  SkillView,
  UndoReport,
} from "../../types";

// The "Lessons" view of the Prime chat (prime_review.rs): pending proposals
// of every project (Review), the project's lessons (Lessons), its yusAi
// skills (Skills, prime_skills.rs) and the refines imported for it, refine
// by refine (Refines). Every action is
// recorded in the lesson history with the author "user"; moving a lesson
// yourself counts as validation.

type Tab = "review" | "lessons" | "skills" | "refines";

type Props = {
  workspacePath: string;
  // Bumped by the pane to re-read (after Remember, for instance).
  refreshKey: number;
  // Something changed (the pane re-reads its pending count).
  onChanged: () => void;
};

// Runs an action, then re-reads the view.
type Act = (action: () => Promise<unknown>) => Promise<void>;

export function PrimeLessonsView({ workspacePath, refreshKey, onChanged }: Props) {
  const [tab, setTab] = useState<Tab>("review");
  const [overview, setOverview] = useState<LessonsOverview | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Re-reads the view; returns the read error, if any.
  const load = useCallback(async () => {
    try {
      setOverview(await api.primeLessonsOverview(workspacePath));
      return null;
    } catch (err) {
      return String(err);
    }
  }, [workspacePath]);

  useEffect(() => {
    void load().then(setError);
  }, [load, refreshKey]);

  // A refused action keeps its error on screen after the re-read.
  const act: Act = useCallback(
    async (action) => {
      let failure: string | null = null;
      try {
        await action();
      } catch (err) {
        failure = String(err);
      }
      const readError = await load();
      setError(failure ?? readError);
      onChanged();
    },
    [load, onChanged],
  );

  const tabs: { id: Tab; label: string; count?: number }[] = [
    { id: "review", label: "Review", count: overview?.proposals.length },
    { id: "lessons", label: "Lessons" },
    { id: "skills", label: "Skills" },
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
        {overview && tab === "review" && <ReviewTab overview={overview} act={act} />}
        {overview && tab === "lessons" && <LessonsTab overview={overview} act={act} />}
        {overview && tab === "skills" && <SkillsTab overview={overview} act={act} />}
        {overview && tab === "refines" && <RefinesTab overview={overview} act={act} />}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- Review

function ReviewTab({ overview, act }: { overview: LessonsOverview; act: Act }) {
  if (overview.proposals.length === 0) {
    return <Empty text="Nothing waits for your review." />;
  }
  return (
    <div className="prime-lessons__list">
      {overview.proposals.map((proposal) => (
        <ProposalCard key={proposal.id} proposal={proposal} act={act} />
      ))}
    </div>
  );
}

function ProposalCard({ proposal, act }: { proposal: ProposalView; act: Act }) {
  const lesson = proposal.lesson;
  const payload = proposal.payload;
  const accept = (level: LessonLevel | null) =>
    void act(() => api.primeAcceptProposal(proposal.id, level));
  const reject = () => void act(() => api.primeRejectProposal(proposal.id));
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
      <div className="prime-lessons__actions">
        {proposal.kind === "promote" && (
          <>
            <ActionButton
              label={proposal.projectType ? `To type ${proposal.projectType}` : "To type"}
              disabled={!proposal.projectType || !lesson}
              title={
                proposal.projectType
                  ? "Share with projects of the same type"
                  : "Choose the project's type first"
              }
              onClick={() => accept("type")}
            />
            <ActionButton
              label="To global"
              primary
              disabled={!lesson}
              onClick={() => accept("global")}
            />
          </>
        )}
        {(proposal.kind === "change" || proposal.kind === "archive") && (
          <ActionButton label="Accept" primary disabled={!lesson} onClick={() => accept(null)} />
        )}
        {proposal.kind === "skill" && payload.action === "delete" && (
          <ActionButton
            label="Archive the skill"
            primary
            title="Archive the yusAi skill this refine deletes, if there is one"
            onClick={() => accept(null)}
          />
        )}
        {proposal.kind === "skill" && payload.action !== "delete" && (
          <>
            <ActionButton
              label="To project"
              primary
              title="Keep the skill in this project"
              onClick={() => accept("project")}
            />
            <ActionButton
              label={proposal.projectType ? `To type ${proposal.projectType}` : "To type"}
              disabled={!proposal.projectType}
              title={
                proposal.projectType
                  ? "Share with projects of the same type"
                  : "Choose the project's type first"
              }
              onClick={() => accept("type")}
            />
            <ActionButton label="To global" onClick={() => accept("global")} />
          </>
        )}
        <ActionButton label="Reject" onClick={reject} />
      </div>
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
        {stringField(entry, "title") ?? stringField(payload, "title") ?? stringField(payload, "id") ?? "Skill"}
      </div>
      <div className="prime-lessons__body">
        {stringField(entry, "content") ?? stringField(payload, "content") ?? ""}
      </div>
      {target && <code className="prime-lessons__code">{target}</code>}
    </div>
  );
}

// ---------------------------------------------------------------- Skills

// Skills reach a thread when it opens (config.skills): project first, then
// the confirmed type, then global. A Python skill whose name another skill
// already holds stays out ("Disabled").
function SkillsTab({ overview, act }: { overview: LessonsOverview; act: Act }) {
  const [showArchived, setShowArchived] = useState(false);
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
      <div className="prime-lessons__toolbar">
        <label className="prime-lessons__toggle">
          <input
            type="checkbox"
            checked={showArchived}
            onChange={(event) => setShowArchived(event.target.checked)}
          />
          Show archived
        </label>
        <span className="prime-lessons__meta">Changes reach threads when they reopen.</span>
      </div>
      {overview.skills.length === 0 && <Empty text="No yusAi skills for this project yet." />}
      {groups.map(({ level, label }) => {
        const inGroup = overview.skills.filter((skill) => skill.level === level);
        if (inGroup.length === 0) return null;
        return (
          <section key={level} className="prime-lessons__group">
            <h3 className="prime-lessons__group-title">{label}</h3>
            {inGroup.map((skill) => (
              <SkillRow key={skill.dir} skill={skill} overview={overview} act={act} />
            ))}
          </section>
        );
      })}
      {showArchived && overview.archivedSkills.length > 0 && (
        <section className="prime-lessons__group">
          <h3 className="prime-lessons__group-title">Archived</h3>
          {overview.archivedSkills.map((archived) => (
            <ArchivedSkillRow
              key={archived.skill.dir}
              archived={archived}
              overview={overview}
              act={act}
            />
          ))}
        </section>
      )}
    </div>
  );
}

function SkillRow({
  skill,
  overview,
  act,
}: {
  skill: SkillView;
  overview: LessonsOverview;
  act: Act;
}) {
  const levels: { level: LessonLevel; label: string; disabled: boolean; title?: string }[] = [
    { level: "project", label: "To project", disabled: false },
    {
      level: "type",
      label: overview.projectType ? `To type ${overview.projectType}` : "To type",
      disabled: !overview.projectType,
      title: overview.projectType ? undefined : "Choose the project's type first",
    },
    { level: "global", label: "To global", disabled: false },
  ];
  return (
    <div className="prime-lessons__card">
      <div className="prime-lessons__card-head">
        <span className="prime-lessons__badge" data-kind="skill">
          {skill.python ? "Python" : "Markdown"}
        </span>
        <span className="prime-lessons__title">{skill.name}</span>
        {skill.python && <code className="prime-lessons__code">{skill.python.importName}</code>}
        {skill.disabled && <span className="prime-lessons__flag">Disabled</span>}
        {skill.invalid && <span className="prime-lessons__flag">Invalid</span>}
        {skill.proposalId && <span className="prime-lessons__flag">From a refine</span>}
      </div>
      <div className="prime-lessons__body">{skill.description}</div>
      {skill.invalid && (
        <div className="prime-lessons__warning">
          Prime does not load this skill: {skill.invalid}.
        </div>
      )}
      {skill.disabled && (
        <div className="prime-lessons__warning">
          {skill.disabled}. Rename one of the two skills (folder, SKILL.md name, Python package).
        </div>
      )}
      <div className="prime-lessons__actions">
        {levels
          .filter((entry) => entry.level !== skill.level)
          .map((entry) => (
            <ActionButton
              key={entry.level}
              label={entry.label}
              disabled={entry.disabled}
              title={entry.title}
              onClick={() =>
                void act(() => api.primeSetSkillLevel(overview.workspaceId, skill.dir, entry.level))
              }
            />
          ))}
        <ActionButton
          label="Show folder"
          title={skill.dir}
          onClick={() => void api.revealAbsolutePath(skill.file).catch(console.error)}
        />
        <ActionButton
          label="Archive"
          danger
          onClick={() => void act(() => api.primeArchiveSkill(skill.dir))}
        />
      </div>
    </div>
  );
}

function ArchivedSkillRow({
  archived,
  overview,
  act,
}: {
  archived: ArchivedSkill;
  overview: LessonsOverview;
  act: Act;
}) {
  return (
    <div className="prime-lessons__card" data-archived="true">
      <div className="prime-lessons__card-head">
        <span className="prime-lessons__badge" data-kind="skill">
          {archived.skill.python ? "Python" : "Markdown"}
        </span>
        <span className="prime-lessons__title">{archived.skill.name}</span>
        <span className="prime-lessons__flag">
          {levelLabel(archived.from.level)} · archived {formatDate(archived.from.archivedMs)}
        </span>
      </div>
      <div className="prime-lessons__body">{archived.skill.description}</div>
      <div className="prime-lessons__actions">
        <ActionButton
          label="Restore"
          title={archived.from.dir}
          onClick={() =>
            void act(() => api.primeRestoreSkill(overview.workspaceId, archived.skill.dir))
          }
        />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- Lessons

function LessonsTab({ overview, act }: { overview: LessonsOverview; act: Act }) {
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
              <LessonRow key={lesson.id} lesson={lesson} overview={overview} act={act} />
            ))}
          </section>
        );
      })}
    </div>
  );
}

function LessonRow({
  lesson,
  overview,
  act,
}: {
  lesson: LessonView;
  overview: LessonsOverview;
  act: Act;
}) {
  const [open, setOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const [history, setHistory] = useState<LessonEventView[] | null>(null);
  useEffect(() => {
    if (!open || history) return;
    api.primeLessonHistory(lesson.id).then(setHistory).catch(console.error);
  }, [open, history, lesson.id]);
  // A change re-reads the history.
  useEffect(() => {
    setHistory(null);
  }, [lesson.updatedAtMs, lesson.status]);
  const ownProject = lesson.workspaceId === overview.workspaceId;
  const active = lesson.status === "active";
  const levels: { level: LessonLevel; label: string; disabled: boolean; title?: string }[] = [
    {
      level: "project",
      label: "To project",
      disabled: !ownProject,
      title: ownProject ? undefined : "This lesson comes from another project",
    },
    {
      level: "type",
      label: overview.projectType ? `To type ${overview.projectType}` : "To type",
      disabled: !ownProject || !overview.projectType,
      title: !overview.projectType
        ? "Choose the project's type first"
        : ownProject
          ? undefined
          : "This lesson comes from another project",
    },
    { level: "global", label: "To global", disabled: false },
  ];
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
      {editing ? (
        <LessonEditor
          lesson={lesson}
          onCancel={() => setEditing(false)}
          onSave={(title, content) => {
            setEditing(false);
            void act(() => api.primeUpdateLesson(lesson.id, title, content));
          }}
        />
      ) : (
        <div className="prime-lessons__body">{lesson.content}</div>
      )}
      {open && (
        <>
          <div className="prime-lessons__actions">
            {active && !editing && (
              <ActionButton label="Edit" onClick={() => setEditing(true)} />
            )}
            {active && (
              <ActionButton
                label={lesson.pinned ? "Unpin" : "Pin"}
                title="Pinned lessons are injected first"
                onClick={() => void act(() => api.primeSetLessonPinned(lesson.id, !lesson.pinned))}
              />
            )}
            {active &&
              levels
                .filter((entry) => entry.level !== lesson.level)
                .map((entry) => (
                  <ActionButton
                    key={entry.level}
                    label={entry.label}
                    disabled={entry.disabled}
                    title={entry.title}
                    onClick={() => void act(() => api.primeSetLessonLevel(lesson.id, entry.level))}
                  />
                ))}
            {active ? (
              <ActionButton
                label="Archive"
                danger
                onClick={() => void act(() => api.primeArchiveLesson(lesson.id))}
              />
            ) : (
              <ActionButton
                label="Restore"
                onClick={() => void act(() => api.primeRestoreLesson(lesson.id))}
              />
            )}
          </div>
          <div className="prime-lessons__history">
            {!history && <div className="prime-chat__status">Loading…</div>}
            {history?.map((event) => (
              <EventLine key={event.id} event={event} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}

function LessonEditor({
  lesson,
  onCancel,
  onSave,
}: {
  lesson: Lesson;
  onCancel: () => void;
  onSave: (title: string, content: string) => void;
}) {
  const [title, setTitle] = useState(lesson.title);
  const [content, setContent] = useState(lesson.content);
  return (
    <div className="prime-lessons__editor">
      <input
        className="prime-lessons__input"
        value={title}
        onChange={(event) => setTitle(event.target.value)}
        placeholder="Title"
      />
      <textarea
        className="prime-lessons__input"
        rows={3}
        value={content}
        onChange={(event) => setContent(event.target.value)}
        onFocus={(event) => {
          const end = event.currentTarget.value.length;
          event.currentTarget.setSelectionRange(end, end);
        }}
        autoFocus
      />
      <div className="prime-lessons__actions">
        <ActionButton
          label="Save"
          primary
          disabled={!content.trim()}
          onClick={() => onSave(title.trim() || lesson.title, content.trim())}
        />
        <ActionButton label="Cancel" onClick={onCancel} />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- Refines

function RefinesTab({ overview, act }: { overview: LessonsOverview; act: Act }) {
  if (overview.refines.length === 0) {
    return <Empty text="No refine imported for this project yet." />;
  }
  return (
    <div className="prime-lessons__list">
      {overview.refines.map((refine) => (
        <RefineRow key={refine.refinementId} refine={refine} act={act} />
      ))}
    </div>
  );
}

function RefineRow({ refine, act }: { refine: RefineView; act: Act }) {
  const [open, setOpen] = useState(false);
  const [detail, setDetail] = useState<RefineDetail | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [undone, setUndone] = useState<string | null>(null);
  useEffect(() => {
    if (!open || detail) return;
    api.primeRefineDetail(refine.refinementId).then(setDetail).catch(console.error);
  }, [open, detail, refine.refinementId]);
  // An undo re-reads the detail.
  useEffect(() => {
    setDetail(null);
  }, [refine.undoneAtMs]);
  const undo = () =>
    void act(async () => {
      setConfirming(false);
      setUndone(undoSummary(await api.primeUndoRefine(refine.refinementId)));
    });
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
      {undone && <div className="prime-lessons__meta">{undone}</div>}
      {open && refine.undoneAtMs === null && (
        <div className="prime-lessons__actions">
          {confirming ? (
            <>
              <ActionButton label="Undo this refine" danger onClick={undo} />
              <ActionButton label="Cancel" onClick={() => setConfirming(false)} />
              <span className="prime-lessons__meta">
                Archives its new lessons and brings back the texts it changed.
              </span>
            </>
          ) : (
            <ActionButton label="Undo" onClick={() => setConfirming(true)} />
          )}
        </div>
      )}
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

function ActionButton({
  label,
  onClick,
  disabled,
  title,
  primary,
  danger,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  primary?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      type="button"
      className="prime-lessons__action"
      data-variant={primary ? "primary" : danger ? "danger" : undefined}
      disabled={disabled}
      title={title}
      onClick={onClick}
    >
      {label}
    </button>
  );
}

// "Undone: 1 lesson archived · 1 text brought back · 1 left as is".
function undoSummary(report: UndoReport): string {
  const parts = [
    report.archived.length && `${report.archived.length} archived`,
    report.reverted.length && `${report.reverted.length} text${report.reverted.length === 1 ? "" : "s"} brought back`,
    report.restored.length && `${report.restored.length} restored`,
    report.rejectedProposals && `${report.rejectedProposals} proposal${report.rejectedProposals === 1 ? "" : "s"} rejected`,
    report.archivedSkills.length && `${report.archivedSkills.length} skill${report.archivedSkills.length === 1 ? "" : "s"} archived`,
    report.restoredSkills.length && `${report.restoredSkills.length} skill${report.restoredSkills.length === 1 ? "" : "s"} restored`,
    report.skipped.length && `${report.skipped.length} left as is (${report.skipped.join("; ")})`,
  ].filter(Boolean);
  return `Undone: ${parts.length ? parts.join(" · ") : "nothing to change"}`;
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
    case "refine:agent":
      return "Model";
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
      return proposal.payload.action === "delete" ? "Delete skill" : "Skill";
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
