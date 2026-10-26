async function load() {
  const res = await fetch("/api/v1/settings");
  if (!res.ok) throw new Error("Failed to load settings");
  const data = await res.json();
  document.getElementById("probe").textContent = data.summary || JSON.stringify(data.probe, null, 2);
  const provider = (data.config && data.config.ai && data.config.ai.reasoning && data.config.ai.reasoning.provider) || "hybrid";
  const radio = document.querySelector(`input[name="ai_mode"][value="${provider}"]`);
  if (radio) radio.checked = true;
  if (data.config) {
    document.getElementById("discovery_root").value = data.config.discovery_root || "";
    document.getElementById("logs_root").value = data.config.logs_root || "";
    const r = data.config.ai && data.config.ai.reasoning;
    if (r && r.routing) {
      document.getElementById("use_cloud_when_no_gpu").checked = !!r.routing.use_cloud_when_no_gpu;
    }
    if (r) {
      document.getElementById("use_ide_provided_models").checked = !!r.use_ide_provided_models;
    }
    if (r && r.always_smallest_local) {
      document.querySelector('input[name="local_model"][value="granite4.1:3b"]').checked = true;
    }
    if (r && r.clouds && r.clouds.openai_compat) {
      document.getElementById("openai_base").value = r.clouds.openai_compat.base_url || "";
    }
    if (data.config.memory) {
      document.getElementById("memory_projection_enabled").checked = !!data.config.memory.projection_enabled;
      const gitStore = data.config.memory.git_store || {};
      document.getElementById("memory_git_store_personal_claims").value = gitStore.personal_claims || "";
    }
  }
}

function memoryQuery() {
  const params = new URLSearchParams();
  const filters = {
    kind: document.getElementById("memory-kind").value,
    status: document.getElementById("memory-status").value,
    visibility: document.getElementById("memory-visibility").value,
  };
  Object.entries(filters).forEach(([key, value]) => {
    if (value) params.set(key, value);
  });
  return params.toString();
}

function memoryFlag(status) {
  if (status === "stale" || status === "superseded" || status === "archived") {
    return status;
  }
  return "current";
}

function renderMemories(memories) {
  const list = document.getElementById("memory-list");
  list.replaceChildren();
  memories.forEach((memory) => {
    const card = document.createElement("article");
    card.className = "memory-card";

    const heading = document.createElement("div");
    heading.className = "memory-card-heading";
    const title = document.createElement("strong");
    title.textContent = memory.title || memory.kind || "Memory";
    const flags = document.createElement("div");
    flags.className = "memory-flags";
    const kind = document.createElement("span");
    kind.className = "memory-flag";
    kind.textContent = memory.kind || "memory";
    const status = document.createElement("span");
    status.className = `memory-flag ${memoryFlag(memory.status)}`;
    status.textContent = memory.status || "current";
    flags.append(kind, status);
    heading.append(title, flags);

    const content = document.createElement("p");
    content.className = "memory-content";
    content.textContent = memory.content || "";

    const footer = document.createElement("div");
    footer.className = "memory-card-footer";
    const details = document.createElement("span");
    details.textContent = [memory.owner, memory.updated_at].filter(Boolean).join(" · ");
    const share = document.createElement("label");
    share.className = "share-toggle";
    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.checked = memory.visibility === "team";
    toggle.setAttribute("aria-label", `Share ${memory.title || memory.id} with team`);
    const shareText = document.createElement("span");
    shareText.textContent = toggle.checked ? "Team" : "Personal";
    toggle.addEventListener("change", async () => {
      toggle.disabled = true;
      const visibility = toggle.checked ? "team" : "personal";
      try {
        const response = await fetch("/api/v1/settings/memory/share", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ id: memory.id, visibility }),
        });
        if (!response.ok) throw new Error((await response.text()) || "Visibility update failed");
        memory.visibility = visibility;
        shareText.textContent = visibility === "team" ? "Team" : "Personal";
      } catch (error) {
        toggle.checked = !toggle.checked;
        document.getElementById("memory-status-message").textContent = String(error);
      } finally {
        toggle.disabled = false;
      }
    });
    share.append(toggle, shareText);
    footer.append(details, share);
    card.append(heading, content, footer);
    list.append(card);
  });
}

async function loadMemories() {
  const message = document.getElementById("memory-status-message");
  message.textContent = "Loading memories…";
  const query = memoryQuery();
  const response = await fetch(`/api/v1/settings/memory${query ? `?${query}` : ""}`);
  if (!response.ok) throw new Error(`Failed to load memories: ${await response.text()}`);
  const data = await response.json();
  if (!data.available) {
    renderMemories([]);
    message.textContent = data.message || "Memory database is not available.";
    return;
  }
  const memories = data.memories || [];
  renderMemories(memories);
  message.textContent = memories.length ? `${memories.length} memories` : "No memories match these filters.";
}

async function exportMemories() {
  const message = document.getElementById("memory-status-message");
  const query = memoryQuery();
  const response = await fetch(`/api/v1/settings/memory/export${query ? `?${query}` : ""}`);
  if (!response.ok) {
    message.textContent = `Export failed: ${await response.text()}`;
    return;
  }
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = "ibis-memories.json";
  link.click();
  URL.revokeObjectURL(url);
  message.textContent = "Memory export downloaded.";
}

document.getElementById("save").addEventListener("click", async () => {
  const status = document.getElementById("status");
  status.textContent = "Saving…";
  const ai_mode = document.querySelector('input[name="ai_mode"]:checked').value;
  const local_model = document.querySelector('input[name="local_model"]:checked').value;
  const body = {
    ai_mode,
    local_model,
    always_smallest_local: local_model === "granite4.1:3b",
    use_cloud_when_no_gpu: document.getElementById("use_cloud_when_no_gpu").checked,
    use_ide_provided_models: document.getElementById("use_ide_provided_models").checked,
    memory_projection_enabled: document.getElementById("memory_projection_enabled").checked,
    memory_git_store_personal_claims: document.getElementById("memory_git_store_personal_claims").value,
    gemini_key: document.getElementById("gemini_key").value,
    openai_key: document.getElementById("openai_key").value,
    openai_base_url: document.getElementById("openai_base").value,
    claude_key: document.getElementById("claude_key").value,
    discovery_root: document.getElementById("discovery_root").value,
    logs_root: document.getElementById("logs_root").value,
  };
  const res = await fetch("/api/v1/settings", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    status.textContent = "Save failed: " + (await res.text());
    return;
  }
  status.textContent = "Saved. Restart Ibis Assistant for reasoning providers to reload.";
  document.getElementById("gemini_key").value = "";
  document.getElementById("openai_key").value = "";
  document.getElementById("claude_key").value = "";
});

document.getElementById("memory-refresh").addEventListener("click", () => {
  loadMemories().catch((error) => {
    document.getElementById("memory-status-message").textContent = String(error);
  });
});
document.getElementById("memory-export").addEventListener("click", exportMemories);
["memory-kind", "memory-status", "memory-visibility"].forEach((id) => {
  document.getElementById(id).addEventListener("change", () => {
    loadMemories().catch((error) => {
      document.getElementById("memory-status-message").textContent = String(error);
    });
  });
});

load().catch((e) => {
  document.getElementById("probe").textContent = String(e);
});
loadMemories().catch((error) => {
  document.getElementById("memory-status-message").textContent = String(error);
});
