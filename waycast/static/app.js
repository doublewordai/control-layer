// Simple health check to test connectivity
async function checkHealth() {
    const statusEl = document.getElementById('status');

    try {
        const response = await fetch('/healthz');
        if (response.ok) {
            statusEl.textContent = 'Connected - Server is running';
            statusEl.className = 'connected';
        } else {
            statusEl.textContent = `Error - Server returned ${response.status}`;
            statusEl.className = 'error';
        }
    } catch (error) {
        statusEl.textContent = `Error - ${error.message}`;
        statusEl.className = 'error';
    }
}

// Check health on load
checkHealth();

// Log to console for debugging
console.log('Control Layer Test Frontend loaded');
console.log('Current path:', window.location.pathname);
