import { useState } from "react"
import HlsPlayer from "../components/HlsPlayer.jsx"

function TestHlsPage() {
  const [logs, setLogs] = useState([])
  const [testCameraId, setTestCameraId] = useState("test")
  const [testStreamUrl, setTestStreamUrl] = useState("")
  
  const addLog = (msg) => {
    const timestamp = new Date().toISOString().split('T')[1]
    setLogs(prev => [...prev.slice(-100), `[${timestamp}] ${msg}`])
    console.log("[TestHls]", msg)
  }
  
  const testPublicStream = () => {
    addLog("Loading public Mux test stream")
    setTestCameraId("mux-test")
    setTestStreamUrl("https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8")
    addLog("✅ Public stream loaded: https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8")
  }
  
  const testLocalStream = () => {
    const cameraId = prompt("Enter camera ID from CameraNode:")
    if (cameraId) {
      addLog(`Loading local CameraNode stream: ${cameraId}`)
      setTestCameraId(cameraId)
      setTestStreamUrl(`http://localhost:8080/hls/${cameraId}/stream.m3u8`)
      addLog(`✅ Local stream configured: http://localhost:8080/hls/${cameraId}/stream.m3u8`)
    }
  }
  
  const testProductionStream = () => {
    const cameraId = prompt("Enter camera ID from production:")
    if (cameraId) {
      addLog(`Loading production stream: ${cameraId}`)
      setTestCameraId(cameraId)
      setTestStreamUrl("")  // Empty means use default backend URL
      addLog(`✅ Production stream configured for camera: ${cameraId}`)
    }
  }
  
  const clearTest = () => {
    setTestStreamUrl("")
    setTestCameraId("test")
    addLog("🧹 Cleared test stream")
  }
  
  const shouldShowPlayer = testCameraId !== "test"
  
  return (
    <div style={{ padding: '20px', maxWidth: '1200px', margin: '0 auto' }}>
      <h1>🧪 HLS Player Test Page</h1>
      <p style={{ color: '#888', marginBottom: '20px' }}>
        Test HLS streams in isolation without auto-refresh or authentication issues
      </p>
      
      <div style={{ 
        background: '#1a1a1a', 
        padding: '15px', 
        borderRadius: '8px', 
        marginBottom: '20px',
        display: 'flex',
        gap: '10px',
        flexWrap: 'wrap'
      }}>
        <button 
          onClick={testPublicStream}
          style={{
            background: '#22c55e',
            color: 'white',
            border: 'none',
            padding: '10px 20px',
            borderRadius: '4px',
            cursor: 'pointer',
            fontWeight: 'bold'
          }}
        >
          Test Public Stream (Mux)
        </button>
        
        <button 
          onClick={testLocalStream}
          style={{
            background: '#3b82f6',
            color: 'white',
            border: 'none',
            padding: '10px 20px',
            borderRadius: '4px',
            cursor: 'pointer',
            fontWeight: 'bold'
          }}
        >
          Test Local CameraNode
        </button>
        
        <button 
          onClick={testProductionStream}
          style={{
            background: '#f59e0b',
            color: 'white',
            border: 'none',
            padding: '10px 20px',
            borderRadius: '4px',
            cursor: 'pointer',
            fontWeight: 'bold'
          }}
        >
          Test Production
        </button>
        
        <button 
          onClick={clearTest}
          style={{
            background: '#6b7280',
            color: 'white',
            border: 'none',
            padding: '10px 20px',
            borderRadius: '4px',
            cursor: 'pointer',
            fontWeight: 'bold'
          }}
        >
          Clear
        </button>
      </div>
      
      <div style={{ 
        display: 'grid', 
        gridTemplateColumns: '1fr 1fr', 
        gap: '20px',
        marginBottom: '20px'
      }}>
        <div>
          <h3>Current Test</h3>
          <div style={{ background: '#0f0f0f', padding: '10px', borderRadius: '4px' }}>
            <p><strong>Camera ID:</strong> {testCameraId}</p>
            <p><strong>Stream URL:</strong> {testStreamUrl || "Production backend (with auth)"}</p>
            <p><strong>Stream Active:</strong> {shouldShowPlayer ? "Yes" : "No"}</p>
          </div>
        </div>
        
        <div>
          <h3>Instructions</h3>
          <div style={{ background: '#0f0f0f', padding: '10px', borderRadius: '4px', fontSize: '14px' }}>
            <p><strong>Public (Mux):</strong> Known-working test stream (no auth)</p>
            <p><strong>Local:</strong> CameraNode on localhost:8080 (no auth)</p>
            <p><strong>Production:</strong> Real camera from backend (with auth)</p>
          </div>
        </div>
      </div>
      
      <div style={{ marginBottom: '20px' }}>
        {shouldShowPlayer && (
          <HlsPlayer 
            key={testCameraId}
            cameraId={testCameraId}
            cameraName={`Test Stream: ${testCameraId}`}
            streamUrl={testStreamUrl}
          />
        )}
        
        {!shouldShowPlayer && (
          <div style={{ 
            background: '#0f0f0f', 
            padding: '40px', 
            borderRadius: '8px', 
            textAlign: 'center',
            color: '#888'
          }}>
            <p style={{ fontSize: '24px', marginBottom: '10px' }}>📹</p>
            <p>No stream loaded. Click a test button above.</p>
          </div>
        )}
      </div>
      
      <div style={{ background: '#0f0f0f', padding: '15px', borderRadius: '8px' }}>
        <div style={{ 
          display: 'flex', 
          justifyContent: 'space-between', 
          alignItems: 'center',
          marginBottom: '10px'
        }}>
          <h3 style={{ margin: 0 }}>Debug Logs</h3>
          <button 
            onClick={() => setLogs([])}
            style={{
              background: '#ef4444',
              color: 'white',
              border: 'none',
              padding: '5px 10px',
              borderRadius: '4px',
              cursor: 'pointer',
              fontSize: '12px'
            }}
          >
            Clear Logs
          </button>
        </div>
        
        <pre style={{ 
          background: '#000', 
          padding: '10px', 
          borderRadius: '4px', 
          maxHeight: '300px',
          overflowY: 'auto',
          fontSize: '12px',
          fontFamily: 'monospace',
          color: '#0f0'
        }}>
          {logs.length === 0 ? "No logs yet..." : logs.join('\n')}
        </pre>
      </div>
      
      <div style={{ 
        marginTop: '20px', 
        padding: '15px', 
        background: '#1e3a5f', 
        borderRadius: '8px',
        fontSize: '14px'
      }}>
        <h4 style={{ margin: '0 0 10px 0' }}>💡 Troubleshooting Tips</h4>
        <ul style={{ margin: 0, paddingLeft: '20px' }}>
          <li><strong>Public stream fails:</strong> Network/firewall issue or HLS.js not loaded</li>
          <li><strong>Local fails:</strong> CameraNode not running, wrong camera ID, or CORS</li>
          <li><strong>Production auth error:</strong> Login first, check Clerk token</li>
          <li><strong>Media error:</strong> Codec mismatch or corrupt segment (check console)</li>
          <li><strong>Check console:</strong> Open browser DevTools (F12) for detailed HLS.js logs</li>
        </ul>
      </div>
    </div>
  )
}

export default TestHlsPage