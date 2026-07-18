// CLI entrypoint: parse argv and dispatch to the store verbs.
import { addTodo, listTodos, markDone } from "./store.js";

function main(argv) {
  const [verb, ...rest] = argv;
  switch (verb) {
    case "add":
      return addTodo(rest.join(" "));
    case "list":
      return listTodos();
    case "done":
      return markDone(Number.parseInt(rest[0], 10));
    default:
      process.stderr.write("usage: todo-tracker <add|list|done> [args]\n");
      process.exitCode = 2;
      return undefined;
  }
}

main(process.argv.slice(2));
