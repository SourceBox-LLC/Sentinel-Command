function Sentinel() {
  return (
    <section className="docs-section" id="sentinel">
      <h2>Sentinel AI agent<a href="#sentinel" className="docs-anchor">#</a></h2>
      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">🛡️</span>
          <span>Sentinel requires a <strong>Pro</strong> or <strong>Pro Plus</strong> plan. Pro: 100 runs/month. Pro Plus: 500 runs/month.</span>
        </p>
      </div>
      <p>
        Sentinel is the optional AI agent that investigates motion events and incidents on your behalf —
        it views the camera, decides whether what it sees warrants attention, files an incident report
        with snapshot evidence, and writes a long-form summary. Configure it on the{" "}
        <a href="/sentinel">Sentinel</a> page; the resulting incidents land in{" "}
        <a href="/incidents">Incident Reports</a> alongside any human-filed reports.
      </p>

      <h3>How it works</h3>
      <p>
        Sentinel is a serverless agent — it sleeps until Command Center wakes it. When a configured
        trigger fires (motion, an incident opened, or a manual "Run now" click), Command Center inserts
        a row into a <code>sentinel_runs</code> queue and POSTs a webhook to the agent. The agent boots,
        drains every pending row across every org, processes each one through an LLM ↔ MCP loop, posts
        results back, and goes back to sleep.
      </p>
      <p>
        One deployed agent serves every org. Per-call org scoping happens at the MCP layer via a signed
        override header — the agent never holds per-org credentials. Each run gets a fresh MCP client
        and a fresh conversation, so cross-org state can't leak between runs.
      </p>

      <h3>What triggers a Sentinel run</h3>
      <p>
        Sentinel only ever runs when something specific tells it to — there's no continuous polling.
        Three trigger types today:
      </p>
      <ul>
        <li>
          <strong>Motion detected</strong> — a camera's FFmpeg scene-change scorer crossed the configured
          threshold (see <a href="#motion-detection">Motion Detection</a>) AND that camera is in scope
          AND the per-camera cooldown window has elapsed AND the schedule allows runs right now.
          Default cooldown is 5 minutes per camera so a busy camera doesn't burn the cap.
        </li>
        <li>
          <strong>Incident opened</strong> — a human filed a new incident from the dashboard. Sentinel
          can investigate further, attach additional snapshots/observations, and finalize the report.
        </li>
        <li>
          <strong>Manual run</strong> — operator clicks "Run now" on the Sentinel page with an optional
          custom prompt and optional camera. Skips the schedule + scope checks (the operator overrode
          them by clicking) but still counts against the monthly cap.
        </li>
      </ul>
      <p>
        Scheduled cron-style sweeps are mapped in the prompt set but not yet wired up — they're a future
        addition.
      </p>

      <h3>What happens during a run</h3>
      <p>
        A typical motion-triggered run looks like this:
      </p>
      <div className="docs-steps">
        <div className="docs-step">
          <div className="docs-step-number">1</div>
          <div className="docs-step-content">
            <h4>Wake + claim the run</h4>
            <p>
              The agent fetches up to 20 pending runs and POSTs <code>/api/sentinel/runs/&#123;id&#125;/start</code>{" "}
              to claim each one — transitions the run from <code>pending</code> to <code>running</code> on
              the Command Center side.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">2</div>
          <div className="docs-step-content">
            <h4>Connect to MCP scoped to this run's org</h4>
            <p>
              Builds an MCP client with the multi-tenant agent key and an{" "}
              <code>X-Agent-Org-Override</code> header pointing at the run's org. All 23 MCP tools are
              available, scoped to that org's cameras, nodes, and incidents.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">3</div>
          <div className="docs-step-content">
            <h4>Run the LLM ↔ MCP loop</h4>
            <p>
              The vision-capable LLM (<code>qwen3.5:cloud</code> by default) is given a trigger-specific
              system prompt and the run's metadata. It typically calls{" "}
              <code>view_camera</code> to see the scene, <code>watch_camera</code> if it needs more
              frames, and <code>create_incident</code> + <code>attach_snapshot</code> + <code>add_observation</code>{" "}
              + <code>finalize_incident</code> if the scene warrants attention. Capped at 10 tool-call
              iterations per run to bound runaway loops.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">4</div>
          <div className="docs-step-content">
            <h4>Post the result</h4>
            <p>
              POSTs <code>/api/sentinel/runs/&#123;id&#125;/complete</code> with a structured outcome:
              <code>incident</code> (with severity + incident_id), <code>no_action</code>, or{" "}
              <code>error</code>. The full tool trace is included so the run drawer in the UI can show
              exactly what happened.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">5</div>
          <div className="docs-step-content">
            <h4>Drain + sleep</h4>
            <p>
              Repeats for any other pending runs in this wakeup, then returns 200. The serverless host
              auto-stops the machine after the connection closes — no idle billing between events.
            </p>
          </div>
        </div>
      </div>

      <h3>Configuration</h3>
      <p>
        Everything lives on the <a href="/sentinel">Sentinel page</a>. The configurable knobs:
      </p>
      <ul>
        <li>
          <strong>Master enable</strong> — kill switch that turns Sentinel off entirely without
          discarding the rest of the configuration.
        </li>
        <li>
          <strong>Trigger toggles</strong> — independently enable motion-triggered runs and
          incident-opened-triggered runs.
        </li>
        <li>
          <strong>Motion cooldown</strong> — minutes between consecutive motion-triggered runs on the
          same camera. Default 5. A busy camera (waving tree, blinking light) without a cooldown
          would burn the monthly cap in hours.
        </li>
        <li>
          <strong>Schedule</strong> — <em>always</em> (24/7), <em>scheduled</em> (within a HH:MM window
          on selected days of the week, in the org's timezone), or <em>off</em>. Wrap-around windows are
          supported (e.g. 22:30 → 06:15 is "after dinner until early morning").
        </li>
        <li>
          <strong>Camera scope</strong> — explicitly include or exclude individual cameras. Cameras
          absent from the scope dictionary default to in-scope, so newly-added cameras don't silently
          fall outside the agent's coverage.
        </li>
      </ul>

      <h3>Per-plan caps</h3>
      <p>
        One "run" = one investigation, regardless of how many tool calls it took. Caps reset on the 1st
        of each calendar month in UTC.
      </p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr>
              <th>Plan</th>
              <th>Monthly runs</th>
              <th>Roughly</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>Free</td>
              <td>0 — Sentinel locked</td>
              <td>n/a</td>
            </tr>
            <tr>
              <td>Pro</td>
              <td><strong>100</strong></td>
              <td>~3 / day, casual home use</td>
            </tr>
            <tr>
              <td>Pro Plus</td>
              <td><strong>500</strong></td>
              <td>~16 / day, commercial-shaped</td>
            </tr>
          </tbody>
        </table>
      </div>
      <p>
        When you hit the cap, dispatch pauses for the rest of the month. There's no overage billing —
        existing recordings, motion alerts, and dashboard playback all keep working as normal. Need
        more than 500/month? Email us and we'll talk.
      </p>

      <h3>Run history and the dashboard</h3>
      <p>
        The <a href="/sentinel">Sentinel page</a> shows recent runs with their trigger type, camera,
        outcome, and duration. Clicking a run opens a drawer with the full agent reasoning trace —
        every tool call, with arguments and (truncated) results — so you can see exactly what the
        agent decided to do and why.
      </p>
      <p>
        Runs that filed an incident link directly to the report on the{" "}
        <a href="/incidents">Incident Reports</a> page, where you can update the status (acknowledged,
        resolved, dismissed), edit the report body, or delete the whole thing.
      </p>

      <h3>Reliability and time bounds</h3>
      <p>
        A single agent run is bounded at every layer to prevent runaway loops or hung calls:
      </p>
      <ul>
        <li><strong>Per-LLM-call timeout</strong> — 120s, configurable. A hung Ollama Cloud call returns clean error rather than wedging the run.</li>
        <li><strong>Per-MCP-tool timeout</strong> — 60s, configurable. A stuck tool surfaces to the LLM as an error result so the model can choose to retry or move on.</li>
        <li><strong>Iteration cap</strong> — 10 tool-call rounds per run. Hits the cap → outcome is <code>error</code> with "investigation incomplete".</li>
        <li><strong>Wall-clock cap</strong> — 540s per wakeup. If the agent times out mid-run, the in-flight run is best-effort marked <code>error</code> on Command Center so it doesn't strand in <code>running</code> state.</li>
        <li><strong>Stranded-run reaper</strong> — runs stuck in <code>running</code> for more than 20 minutes are automatically reaped to <code>error</code>. Catches the rare case where the agent process crashes before its own cleanup wrapper fires.</li>
      </ul>

      <h3>Privacy and data flow</h3>
      <div className="docs-callout docs-callout-warning">
        <p>
          <span className="docs-callout-icon">⚠️</span>
          <span>
            Sentinel is the one part of SourceBox Sentry that sends camera
            snapshots <em>out of your network</em> — to whichever LLM endpoint
            is configured (Ollama Cloud by default). Without that the model
            can't see what triggered the run. <strong>If you don't want any
            footage leaving your hardware, leave Sentinel disabled</strong> —
            motion detection, recording, and notifications all work without it.
          </span>
        </p>
      </div>
      <ul>
        <li><strong>Snapshots only when investigating.</strong> Sentinel grabs an ephemeral JPEG via <code>view_camera</code> at the moment of a run — it doesn't ship a continuous stream. Recordings (the persistent video archive) stay on your CloudNode regardless.</li>
        <li><strong>Trigger-driven only.</strong> Sentinel fires only on triggers <em>you</em> configure (motion / incident_opened / manual). No background polling, no continuous monitoring.</li>
        <li><strong>LLM endpoint is yours to point.</strong> The default is Ollama Cloud, but the agent works against a self-hosted Ollama just as well — set <code>OLLAMA_HOST</code> to your own URL and snapshots never leave infrastructure you control.</li>
        <li><strong>Where the agent runs.</strong> The agent process itself runs on hardware we operate (Fly.io, US region). The auth model is two shared secrets (run-callback + MCP bearer) that scope to whichever org each run was dispatched for.</li>
      </ul>

      <h3>Troubleshooting</h3>
      <ul>
        <li>
          <strong>"Sentinel is configured but nothing fires."</strong> Check the master enable toggle,
          then the per-trigger toggles. Then verify the camera is in scope (default is in-scope, so
          this only matters if you've explicitly set it to false). Then check whether the schedule
          window allows runs at the current time in your org's timezone. Finally, check the cap —
          the bottom of the Sentinel page shows runs used vs. cap.
        </li>
        <li>
          <strong>"Motion fires every few seconds and burns through my cap."</strong> Bump the motion
          cooldown to a higher value (15-30 min for noisy environments). Or scope the camera out and
          rely on incident-opened triggers + manual runs.
        </li>
        <li>
          <strong>"A run is stuck in <em>running</em> state."</strong> The wall-clock-timeout cleanup
          marks it as <code>error</code> if the agent's wrapper fires, and the reaper marks it as
          <code>error</code> if it doesn't fire within 20 minutes. Refresh the page.
        </li>
        <li>
          <strong>"The agent filed an incident I disagree with."</strong> Open it from{" "}
          <a href="/incidents">Incident Reports</a> and update the status to <em>dismissed</em>, or
          delete the whole row. The agent's reasoning lives in the run drawer if you want to see why
          it decided what it did.
        </li>
      </ul>
    </section>
  )
}

export default Sentinel
