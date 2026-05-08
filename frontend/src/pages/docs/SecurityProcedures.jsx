import { Link } from "react-router-dom"


function SecurityProcedures() {
  return (
    <section className="docs-section" id="security-procedures">
      <h2>Security Procedures<a href="#security-procedures" className="docs-anchor">#</a></h2>
      <p>Step-by-step guides for handling security incidents. Act quickly to minimize exposure.</p>

      <h3>Compromised MCP API Key</h3>
      <p>If you suspect an MCP API key has been leaked, shared, or used by an unauthorized party:</p>
      <ol>
        <li><strong>Revoke the key immediately</strong> — Go to the <Link to="/mcp">MCP Control Center</Link>, find the key, and click <strong>Revoke</strong>. This takes effect instantly.</li>
        <li><strong>Review MCP activity logs</strong> — Go to <Link to="/admin">Admin Dashboard</Link> and check the <strong>MCP Tool Activity</strong> section. Filter by the compromised key name to see exactly which tools were called, when, and what data was accessed.</li>
        <li><strong>Generate a new key</strong> — Create a replacement key in the MCP Control Center and update your AI client configuration.</li>
        <li><strong>Check for unusual access</strong> — Look for unexpected <code>view_camera</code> or <code>watch_camera</code> calls that may indicate someone was viewing your camera feeds.</li>
      </ol>

      <h3>Compromised CloudNode API Key</h3>
      <p>If a CloudNode API key is compromised, an attacker could potentially push video segments to your storage:</p>
      <ol>
        <li><strong>Rotate the key</strong> — Go to <Link to="/settings">Settings</Link>, find the node, and click <strong>Rotate Key</strong>. The old key is invalidated immediately.</li>
        <li><strong>Update the CloudNode</strong> — The CloudNode will disconnect. Re-run setup with the new API key.</li>
        <li><strong>Review audit logs</strong> — Check stream access logs in the <Link to="/admin">Admin Dashboard</Link> for unusual activity.</li>
        <li><strong>Verify video integrity</strong> — If you suspect tampered footage, check your CloudNode logs for upload activity you don't recognize.</li>
      </ol>

      <h3>Compromised User Account</h3>
      <p>If a Clerk user account in your organization is compromised:</p>
      <ol>
        <li><strong>Remove the user</strong> — Go to your Clerk dashboard and remove the user from the organization or disable their account.</li>
        <li><strong>Revoke all MCP keys</strong> — If the user had admin access, they may have created MCP API keys. Revoke all keys in the <Link to="/mcp">MCP Control Center</Link> and regenerate only the ones you need.</li>
        <li><strong>Rotate CloudNode keys</strong> — If the user had <code>manage_cameras</code> permission, rotate all node API keys from <Link to="/settings">Settings</Link>.</li>
        <li><strong>Review all logs</strong> — Check both stream access logs and MCP activity logs in the <Link to="/admin">Admin Dashboard</Link> for the affected time period.</li>
      </ol>

      <h3>Suspicious Camera Access</h3>
      <p>If you see unexpected entries in your stream access logs:</p>
      <ol>
        <li><strong>Identify the source</strong> — Check the user email, IP address, and timestamp in <Link to="/admin">Admin Dashboard</Link> &gt; Stream Access Logs.</li>
        <li><strong>Check MCP activity</strong> — If the access came from an MCP tool, the MCP Tool Activity section will show which API key was used.</li>
        <li><strong>Revoke access</strong> — Remove the user from your Clerk organization or revoke the MCP key, depending on the source.</li>
        <li><strong>Enable scheduled recording</strong> — If you don't need 24/7 access, restrict streaming to specific hours from <Link to="/settings">Settings</Link>.</li>
      </ol>

      <h3>General Security Best Practices</h3>
      <ul>
        <li><strong>Rotate keys regularly</strong> — Rotate CloudNode and MCP API keys periodically, even without an incident.</li>
        <li><strong>Use separate MCP keys</strong> — Create a unique MCP API key for each AI client so you can revoke individually.</li>
        <li><strong>Monitor the Admin Dashboard</strong> — Review stream access logs and MCP activity regularly for anything unexpected.</li>
        <li><strong>Keep CloudNode updated</strong> — Always run the latest version for security patches.</li>
        <li><strong>Limit organization members</strong> — Only invite users who need access. Use Clerk roles to restrict permissions.</li>
      </ul>
    </section>
  )
}

export default SecurityProcedures
