const root = document.querySelector("#groups");

function render(snapshots) {
  root.replaceChildren();
  if (!snapshots.length) {
    root.textContent = "No recorded groups yet. Keep the extension installed while groups are open.";
    return;
  }
  for (const snapshot of snapshots.filter((item) => item.title)) {
    const button = document.createElement("button");
    button.textContent = `${snapshot.title} (${snapshot.tabs.length} tabs)`;
    button.addEventListener("click", async () => {
      button.disabled = true;
      const result = await chrome.runtime.sendMessage({ type: "restoreClosedGroup", groupTitle: snapshot.title });
      if (!result?.ok) {
        button.disabled = false;
        root.className = "error";
        root.insertAdjacentText("beforeend", `\nRestore refused: ${result?.error || "unknown_error"}`);
      } else {
        button.textContent = `${snapshot.title} restored (${result.data.tabCount} tabs)`;
      }
    });
    root.append(button);
  }
}

chrome.runtime.sendMessage({ type: "listSnapshots" }).then((result) => {
  if (!result?.ok) throw new Error(result?.error || "snapshot_read_failed");
  render(result.snapshots);
}).catch((error) => {
  root.className = "error";
  root.textContent = error.message;
});
