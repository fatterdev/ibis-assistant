"use strict";

/**
 * "Quick Actions" sidebar view: the command surfaces that lived only in the
 * command palette and the status bar, one click away next to Memory in the
 * Ibis Assistant container (owner request 03/11).
 */

const vscode = require("vscode");

const ACTIONS = [
  {
    label: "Extension Settings",
    icon: "settings-gear",
    command: "ibis.openExtensionSettings",
    description: "Runtime mode, binary, data dir, defaults",
  },
  {
    label: "Runtime Settings UI",
    icon: "gear",
    command: "ibis.openSettings",
    description: "Open the runtime's own settings page",
  },
  {
    label: "Status",
    icon: "info",
    command: "ibis.status",
    description: "Effective backend, bridge and provider state",
  },
  {
    label: "Show Output",
    icon: "output",
    command: "ibis.showOutput",
    description: "The Ibis Assistant output channel",
  },
  {
    label: "Get Started walkthrough",
    icon: "book",
    command: "ibis.openWalkthrough",
    description: "Onboarding in four steps",
  },
];

class QuickActionsProvider {
  /**
   * @param {{ label: string, icon: string, command: string, description: string }} action
   * @returns {vscode.TreeItem}
   */
  getTreeItem(action) {
    const item = new vscode.TreeItem(action.label, vscode.TreeItemCollapsibleState.None);
    item.iconPath = new vscode.ThemeIcon(action.icon);
    item.description = action.description;
    item.command = { command: action.command, title: `Ibis Assistant: ${action.label}` };
    item.contextValue = "ibisQuickAction";
    return item;
  }

  /** @returns {Array<{ label: string, icon: string, command: string, description: string }>} */
  getChildren() {
    return ACTIONS;
  }
}

/**
 * Wire the Quick Actions view and its walkthrough deep link.
 * @param {vscode.ExtensionContext} context
 */
function registerQuickActions(context) {
  const provider = new QuickActionsProvider();
  context.subscriptions.push(
    vscode.window.createTreeView("ibis.quickActions", { treeDataProvider: provider }),
    vscode.commands.registerCommand("ibis.openWalkthrough", async () => {
      await vscode.commands.executeCommand(
        "workbench.action.openWalkthrough",
        "fatterdev.ibis-assistant#ibis-onboarding",
      );
    }),
  );
}

module.exports = { registerQuickActions };