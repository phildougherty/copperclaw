// JSON-backed todo store. One responsibility: persistence + the verbs
// that mutate/read it. The CLI layer (index.js) does argv parsing only.
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const DB_PATH = process.env.TODO_DB ?? "todos.json";

function load() {
  if (!existsSync(DB_PATH)) return [];
  try {
    return JSON.parse(readFileSync(DB_PATH, "utf8"));
  } catch {
    // A corrupt store is a user-reachable state, not an impossible one:
    // start clean rather than crashing on every subsequent invocation.
    return [];
  }
}

function save(todos) {
  writeFileSync(DB_PATH, JSON.stringify(todos, null, 2));
}

export function addTodo(text) {
  if (!text || !text.trim()) {
    process.stderr.write("cannot add an empty todo\n");
    process.exitCode = 1;
    return;
  }
  const todos = load();
  todos.push({ id: todos.length + 1, text: text.trim(), done: false });
  save(todos);
}

export function listTodos() {
  const todos = load();
  if (todos.length === 0) {
    process.stdout.write("no todos yet\n");
    return;
  }
  for (const t of todos) {
    process.stdout.write(`${t.done ? "[x]" : "[ ]"} ${t.id}. ${t.text}\n`);
  }
}

export function markDone(id) {
  const todos = load();
  const todo = todos.find((t) => t.id === id);
  if (!todo) {
    process.stderr.write(`no todo with id ${id}\n`);
    process.exitCode = 1;
    return;
  }
  todo.done = true;
  save(todos);
}
