# Code sessions on the phone: projects and order

Status: implemented on `feat/mobile-session-order`, branched from `upstream/staging` (`4db02a6`). The Changes panel is still out of scope.

## Goal

On Patchbay Remote, optionally group Code sessions by project, and let the person arrange those projects by dragging. The list on the home screen and the open session should both be able to do it. The desktop already arranges projects in the sidebar that stays beside the open session (`feat/manual-project-order`, PR #131). The phone shows either the list or the session, never both, so the same control has to exist in both places.

The first version is display and order only. No new gateway method, no database column, no mesh sync.

## What the desktop actually reorders

`src/code/CodeSidebar.tsx` does not reorder sessions. It reorders project folders.

- A project is one folder. The key is `cwd` on this computer, and `host:cwd` when the folder is on another computer. The same path on two computers is two projects.
- Pinned sessions sit in their own section, above the folders. They are not in a folder and they are not dragged.
- Inside a folder, sessions stay newest first (`updatedAt`). That order is not manual.
- Folder order is a string array in `localStorage` under `patchbay.codeSidebar.order`. A folder seen for the first time goes on top, then stays where it was put. A folder that drops out of view (everything in it archived, or its computer offline) keeps its slot.
- The header is the drag handle. Four pixels of movement starts a drag; a click that wasn't a drag collapses the folder. Alt+Up and Alt+Down move it without a pointer. A line shows the slot. Searching shows matches even inside a collapsed folder.
- The "+" on a header starts a session in that folder, on that computer.

Order does not sync. Another window on the same computer shares it only because they share `localStorage`. A phone will have its own order, which is what we want: arranging a phone list should not shuffle the sidebar on a computer.

## What the phone does now

The Code tab and the in-session sheet both render `CodeSessionList` (`src/remote/CodeSessionList.tsx`). Grouping is off until Settings → “Group code sessions by project” (`patchbay.remote.groupCodeByProject`). Off, the list is computers, online first, pinned then newest, which is how it looked before this. On, projects nest under each computer. The grip reorders projects within that computer, a hold on the name moves one up or down, a tap collapses it, and “+” opens a new session already pointed at that computer and folder (hidden when the computer is offline). Pinned sessions move to a Pinned section above the computers. The open session’s row is marked in the sheet.

Order is `patchbay.remote.codeProjectOrder` on this phone only. `src/lib/manualOrder.ts` is the same prepend-and-move rules the desktop sidebar uses. Opening a session still replaces the list; the header’s list button brings the same list back as a sheet, and picking a row switches sessions.

## Grouping

Keep today's computer list as the default. A phone's first question is which computer, and whether it is online. Add a switch in the phone's Settings sheet, stored like the lock and background flags:

`patchbay.remote.groupCodeByProject`, default off.

When it is on, nest projects under each computer. Do not flatten into one folder list the way the desktop does. A project cannot move to another computer by being dragged, and offline belongs on the computer header. The project key is always `${hostNode}:${cwd}`. Do not reuse `remoteHost()` for this. On a desktop that function returns null for a session hosted on the machine you're sitting at, so the key is just `cwd`. On a phone every session is hosted somewhere else, but `remoteHost()` also returns null until mesh state has loaded, and two computers with the same path must never collapse into one group.

A computer with one project still gets a project header, so there is something to drag and collapse.

Pinned sessions, while grouping is on, move to a Pinned section above the computers, with the project name as the second line. That matches the desktop. While grouping is off, leave the star and the "pinned first within the computer" sort alone.

Sessions inside a project stay newest first. Do not add a drag handle to a session row. That row already uses a hold for its action sheet, and a vertical drag fights the list scroll. The desktop doesn't reorder sessions either.

## Arranging

One order array per phone, `patchbay.remote.codeProjectOrder`, same rules as the sidebar: unseen projects prepend, then stick, including projects whose sessions are all archived. Order is among projects of one computer. The visible list passed to the move is that computer's projects, not the whole screen.

Computers stay "online first". Reordering computers is a different question and not part of this.

The drag handle is a grip on the left of the project header, not the session row and not the whole header. A tap on the name still collapses the project, and the "+" still starts a session. The desktop drags from the whole header because that row is not also a place you scroll past with a finger. On the phone the grip is the part that captures the touch (`setPointerCapture`, `touch-action: none`) once the pointer has moved a few pixels, so the list can still scroll from anywhere else.

There is no Alt+arrow. Hold the header for move up and move down, the same hold the session rows already use for their actions.

Collapse is the other half of grouping and should ship with it. `patchbay.remote.codeProjectCollapsed` is a set of project keys, same as `patchbay.codeSidebar.collapsed`. The phone has no sidebar filter, so collapse is the way to hide a project. Don't build search in this version.

The "+" on a header opens `NewSessionSheet` with that computer and folder already chosen. The sheet needs an optional initial host and folder; today it resets both when the host effect runs (`setFolder(o.recent[0])`).

## In the session, not only on the list

Extract the Code tab body (computer sections, optional project sections, order, collapse) so `Home` and a sheet can both render it. The sheet opens from the session header, next to the back button, and lists the other sessions with the same grouping and the same drag. Picking one switches `activeCodeSessionId` and re-attaches, which `CodeView` already does when that id changes. Closing the sheet leaves the current session.

Don't put a second copy of the order algorithm in the sheet. One `localStorage` key, one move function.

The desktop sidebar can keep its own component. Sharing the phone's list with it would drag the phone's computer sections and hold menus into the window. Sharing the small piece is worth it: given the visible keys and the saved array, produce the ordered keys, insert first-seen keys at the front, and move one key to a slot. That is the part that is easy to get wrong twice (`moveFolder` in `CodeSidebar.tsx`). A hook used by the sidebar and the phone is enough. The gesture code stays in each view.

## What this does not need

- No gateway RPC and no change to `remote.index`. The phone already has `cwd`, `hostNode`, `pinned`, `updatedAt`, `archivedAt`.
- No sync of the order or the toggle. Same reason a voice preference stays on the device that set it: it is this screen's arrangement, not the session's.
- No per-session manual order.
- No delete, terminal, move, or take over. Those stay desktop-only, as in `docs/design/mobile.md`.

## Also visible once you're in the session

The Changes button is on the phone header, and the panel is a side column: `w-[42%] min-w-80` in `CodeView`. On a phone that column is wider than the screen, so the transcript and the diff sit in a row that doesn't fit. Desktop does not sort that file list; `collectChanges` follows the order the tool calls arrived. There is no sort behavior to port. If Changes stays on the phone, it should be a full-height sheet over the transcript, not a column beside it. That is separate from project order, and worth doing because the button is already there.

## Done when

- With the switch off, the Code tab looks as it does today.
- With it on, each computer lists its projects. A new project appears at the top of that computer and then stays put, including across a restart of the app. Dragging a header, or move up / move down, reorders projects only within that computer. Collapsing a project hides its sessions and survives a restart.
- The same list, with the same order, opens from a session that is already on screen, and choosing a row switches to that session.
- "+" on a project starts a session in that folder on that computer.
- `pnpm dev:remote` is the check. The fake backend already returns Code sessions. Confirm the default list, the switch, a drag, a collapse, the in-session sheet, and a prefilled new session. Then the same on a phone or an emulator, because WebView scrolling is the part a desktop browser will not catch.
