# Using Sentinel

How to set up and use **Sentinel by SourceBox**: connecting cameras, watching and recording, alerts, incidents, Sentinel AI, integrations, your team and your data. It describes the hosted app at [app.sentinel-command.com](https://app.sentinel-command.com). A self-hosted Command Center works the same way, with the differences noted.

## Contents

1. [Getting started](#getting-started)
2. [Watching your cameras](#watching-your-cameras)
3. [Recording and storage](#recording-and-storage)
4. [Notifications and email](#notifications-and-email)
5. [Incidents](#incidents)
6. [Sentinel AI](#sentinel-ai)
7. [AI assistants (MCP)](#ai-assistants-mcp)
8. [Home Assistant](#home-assistant)
9. [Your team](#your-team)
10. [Plans and limits](#plans-and-limits)
11. [Activity logs](#activity-logs)
12. [Your data and your account](#your-data-and-your-account)
13. [Troubleshooting](#troubleshooting)

**Who can do what.** Everything in Sentinel belongs to an *organization*. **Admins** can use every page. **Members** can watch live video, read the notification inbox, and view (not change) Sentinel AI; the Settings, Incidents, Integrations, MCP and Admin pages are admin-only.

---

## Getting started

You need a computer with one or more **USB cameras** plugged into it: a Linux machine (a Raspberry Pi works), a Mac or a Windows PC. That computer runs **CameraNode**, our free software that records your cameras and sends live video to your dashboard.

1. **Sign up** at [app.sentinel-command.com](https://app.sentinel-command.com/sign-up) and create an organization. You become its admin.
2. **Add a node.** Go to **Settings → Camera Nodes** and click **Add Node** (on a new account, **Add Your First Node**). Give it a name, such as "Garage".
3. **Save the credentials.** The node's API key is shown **once**. The dialog builds an install command with it included.
4. **Install CameraNode.** Run the command the dialog shows on the computer your cameras are plugged into. Windows uses an installer instead; the dialog explains both. For a machine that runs unattended, use the **unattended** command, which installs CameraNode as a background service.
5. **Wait about a minute.** CameraNode finds your cameras and connects. They appear on the **Dashboard**, and the node shows as **Online** in Settings.

CameraNode only makes *outbound* connections to Command Center, so there are no ports to open on your router.

More about CameraNode itself (its own local dashboard, modes, and commands): [CameraNode guide](/camera-node/README.md).

## Watching your cameras

The **Dashboard** shows every camera in your organization, with its status, the node it's on, and a live view.

- **Live video** starts when you open the page. Each camera tile has **Fullscreen**, **Snapshot** and **Record** buttons.
- **Snapshot** asks the camera for a still image now. It is saved on the CameraNode.
- **Record** turns continuous recording on or off for that camera (see below).
- **Viewer-hours.** Every hour of live video your organization watches counts towards a monthly allowance, shown under **Viewer hours this month** in the sidebar. Recording, snapshots and motion detection don't count. Live video through Home Assistant on your own network doesn't count either.

## Recording and storage

**Recordings never leave your CameraNode.** They are kept in an encrypted database on that computer, not in our cloud.

- **Turn recording on** with the **Record** button on a camera tile, or in **Settings → Camera Nodes**, where each camera has two options:
  - **Continuous 24/7**: records all the time.
  - **Scheduled Recording**: records only between the times you set. A camera uses one or the other.
- **Set your time zone** in **Settings → Time Zone**, so "08:00–17:00" means your local time. It starts as UTC.
- **Watching recordings.** Open CameraNode's own web dashboard on the computer it runs on and go to its **Recordings** tab. Its **Snapshots** tab has your saved stills.

### Storage cap

Each node keeps recordings up to a **storage cap** (64 GB unless you chose otherwise during setup). When the cap is reached, the oldest recordings are deleted to make room. CameraNode checks every 5 minutes.

You can see each node's usage in **Settings → Camera Nodes**, under **Storage**. To change the cap:

- **From Command Center:** click **Change** next to the node's storage bar, enter a size in GB and click **Save**. The node must be online and running CameraNode 0.1.79 or later.
- **On the node itself:** open CameraNode's web dashboard and go to its **Storage** page (CameraNode 0.1.78 or later).

Lowering the cap deletes the oldest recordings straight away, and both screens tell you roughly how much before you save. The cap can't be larger than the node's disk.

## Notifications and email

The **bell** at the top of the page is your organization's inbox: cameras and nodes going offline and coming back, motion, new incidents, membership changes and more.

**Settings → Notifications** chooses which events appear in the inbox. Turning one off only hides the notification; motion is still recorded in history.

**Settings → Email Alerts** chooses which events also send an email:

| Alert | Who gets it | Default |
| --- | --- | --- |
| Camera offline / recovered (no contact for 90 seconds) | All members | On |
| CameraNode offline / recovered | Admins | On |
| An incident was created | All members | On |
| MCP key created or revoked | Admins | On |
| CameraNode disk almost full | Admins | On |
| Member added, role changed, or removed | Admins | On |
| Motion detection | All members | **Off** |

Motion email is built not to flood you. The first motion from a camera emails at once. Anything else in the next ~15 minutes is collected into one summary email. Every email has a one-click **unsubscribe** link for that kind of alert.

## Incidents

An **incident** is a report about something that happened: a title, a severity, a status, a written report, and **evidence** (snapshots, short video clips and notes). Incidents are filed by people, by Sentinel AI, or by an AI assistant you've connected.

- Open **Incidents** to see them. Filter by status (open by default), and by who filed them (**Anyone**, **AI**, **Human**).
- **New Incident** files one yourself.
- Click an incident to read the report, step through its evidence and change its status: **open**, **acknowledged**, **resolved** or **dismissed**.
- **Evidence is stored in Command Center** (unlike recordings) until you delete the incident.

## Sentinel AI

Sentinel AI is an optional agent that investigates for you. When something happens, it looks at your cameras, decides whether it matters, and files an incident with snapshots, clips and a written summary. It's included with **Pro** (100 runs a month) and **Pro Plus** (500). On a self-hosted install it needs a licence key.

**It is off until an admin turns it on.** While it is on, it sends camera images and incident details to our AI provider ([Ollama Cloud](https://ollama.com)) so the model can describe what it sees. Nothing is sent while it is off. See the [Privacy Policy](https://sentinel-command.com/legal/privacy#sentinel-ai).

On the **Sentinel AI** page:

- **The switch in the header** turns Sentinel on or off. **Run now** starts one investigation immediately, with your own instructions if you like. It's available only while Sentinel is on.
- **Configure → Triggers**: what wakes it.
  - **Motion detected** uses each camera's normal motion threshold.
  - **Incident opened by a human** makes Sentinel collect supporting evidence for a report someone filed.
- **Configure → Schedule**: **Always on**, **Scheduled** (only within a weekly window, for example overnight), or **Off**.
- **Configure → Camera scope**: which cameras Sentinel may look at. Cameras outside the scope are ignored completely. Use this to keep private spaces off its desk.
- **Overview** and **History** show what it has done: each run, what triggered it, its outcome, and any incident it filed.

**AI makes mistakes.** Sentinel can misidentify people and events or miss them. Treat its reports as leads to check, not as facts.

Runs reset on the 1st of each month (UTC). When the month's runs are used up, Sentinel pauses until then; nothing else stops.

## AI assistants (MCP)

Command Center has an [MCP](https://modelcontextprotocol.io) server, so AI assistants such as Claude can list your cameras, look at live frames, and review or file incidents. It's included with Pro and Pro Plus.

1. Open **MCP** and create a key under **API Keys**. Choose what it may do:
   - **All tools**;
   - **Read-only** (it can look, but not change anything);
   - **Custom**: tools you pick from the read and write lists.
2. Follow **Connect a Client** to add Sentinel to your assistant. There are instructions for Claude Desktop, Claude Code, Cursor and Windsurf.
3. Watch what your assistant does in **Live Activity**. Every call is logged.

The key is shown **once**. Anyone with it can do whatever it allows, so keep it secret and revoke keys you don't use. The assistant's provider receives what it reads, including camera images, under that provider's terms.

## Home Assistant

The [Sentinel Home Assistant integration](https://github.com/SourceBox-LLC/Sentinel-HomeAssistant) brings every camera into Home Assistant, with live video, snapshots, recording control and motion. It works on every plan.

Go to **Integrations**, click **Generate Key**, and enter your Command Center URL and the key in Home Assistant. Live video goes straight from each node over your local network, so it doesn't use viewer-hours.

## Your team

Invite people and manage roles from the **organization menu** at the top of the page: choose **Manage**, then **Members**.

- **Admins** manage cameras, nodes, settings, keys, billing and data.
- **Members** watch live video and see notifications.
- A member who needs admin access can click **Request it** under "Need admin access?" on the Dashboard's welcome panel; admins get a notification.

Your plan sets how many people an organization can have. A self-hosted install has a single admin account.

## Plans and limits

The current plans and prices are on the [Pricing page](https://app.sentinel-command.com/pricing). Plans differ mainly in **viewer-hours**: how much live video your organization can watch each month. They also differ in the number of cameras, nodes and members, Sentinel AI runs, and how long logs are kept.

- **We never charge overages.** When a limit is reached, that feature pauses until the 1st of the next month (UTC). For example, live playback stops while cameras keep recording.
- **More cameras than your plan allows** (after a downgrade, say): the oldest cameras keep streaming and the rest are suspended. Nothing is deleted, and upgrading brings them back.
- **A failed payment** gives you a 7-day grace period on your current plan, during which you can't add nodes or create keys. After that, Free plan limits apply until payment succeeds.

Manage your subscription from **Manage Plan** in the sidebar or the **Pricing** page.

## Activity logs

The **Admin** page has your organization's records, each with filters and **Export CSV**:

- **Stream Access**: who watched which camera, when, and from which IP address.
- **Organization Audit**: security-relevant actions, such as keys created and settings changed.
- **MCP Activity**: every call an AI assistant made.
- **Motion**: motion events by camera.

Logs are deleted automatically after 30, 90 or 365 days, depending on your plan.

## Your data and your account

All of these are in **Settings** and are admin-only unless noted.

- **Download your data:** **Privacy & Data → Download my data (ZIP)**. It contains everything Command Center holds for your organization, including incidents with their images and clips, and Sentinel AI runs. Recordings aren't included, because they live on your CameraNodes.
- **Wipe All Logs** (Pro and Pro Plus): deletes stream access logs, MCP activity logs and usage statistics.
- **Full Organization Reset** deletes everything Command Center holds for the organization: nodes, cameras, incidents, logs and settings. It also tells each connected CameraNode to **wipe its local recordings**. It's available on every plan and can't be undone.
- **Delete your account** (any member): choose **Delete account** in the account menu (your picture, top right). Before you confirm, it shows what will happen in each of your organizations:
  - organizations where you're the only member are deleted with your account;
  - if you're the only admin of an organization with other members, make someone else an admin first;
  - in the organizations you leave, your viewing history is deleted and audit entries show "deleted user".

  If you pay for a plan, cancel it first.

Details: [Privacy Policy](https://sentinel-command.com/legal/privacy) · [Terms of Service](https://sentinel-command.com/legal/terms).

## Troubleshooting

- **A camera shows but has no video:** [Video not showing](/camera-node/docs/runbooks/video-not-showing.md).
- **A node shows Offline:** check that the computer is on and connected to the internet, and that CameraNode is running. The node comes back by itself when it reconnects.
- **"Update available" on a node:** re-run the install command from **Add Node** on that computer, or download the latest release from the [CameraNode releases page](https://github.com/SourceBox-LLC/Sentinel-CameraNode/releases/latest).
- **Anything else:** email [support@sentinel-command.com](mailto:support@sentinel-command.com).
