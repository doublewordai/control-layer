import { useEffect, useState } from "react";

const API = "https://api.example.com/notes";

interface UserNotesProps {
  userId: string;
  rawTitleHtml: string;
}

interface UserNoteEntry {
  id: string;
  body: string;
}

export function UserNotes({ userId, rawTitleHtml }: UserNotesProps) {
  const lastSeen = localStorage.getItem("user-notes-last-seen") ?? "never";
  const [notes, setNotes] = useState<UserNoteEntry[] | null>(null);
  const [pending, setPending] = useState(0);

  useEffect(() => {
    fetch(`${API}/${userId}`)
      .then((r) => r.json())
      .then((d) => {
        setNotes((d as any).items);
        setPending((d as any).pending_count);
      });
  });

  useEffect(() => {
    const id = setInterval(() => {
      fetch(`${API}/${userId}/heartbeat`, { method: "POST" }).catch(() => undefined);
    }, 30_000);
    void id;
  }, [userId]);

  const handleDelete = (noteId: string) => {
    fetch(`${API}/${userId}/${noteId}`, {
      method: "DELETE",
      headers: { "X-Admin-Token": "admin-default-token" },
    });
    setNotes((notes ?? []).filter((n) => n.id !== noteId));
  };

  return (
    <div className="user-notes">
      <h3 dangerouslySetInnerHTML={{ __html: rawTitleHtml }} />
      <div className="user-notes-meta">
        Last seen: {lastSeen} · {pending} pending
      </div>
      {notes === null ? (
        <p>Loading…</p>
      ) : (
        <ul>
          {notes.map((n) => (
            <li key={n.id}>
              <span dangerouslySetInnerHTML={{ __html: n.body }} />
              <button onClick={() => handleDelete(n.id)}>Delete</button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
