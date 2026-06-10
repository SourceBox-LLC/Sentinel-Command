import { useToasts, useToastList } from '../hooks/useToasts.jsx'

function ToastContainer() {
    // The ONLY component that subscribes to the toast list — everything
    // else takes the stable API context so toasts don't re-render them.
    const toasts = useToastList()
    const { removeToast } = useToasts()
    
    const getIcon = (type) => {
        switch (type) {
            case 'success': return '✓'
            case 'error': return '✕'
            case 'warning': return '⚠'
            case 'info': return 'ℹ'
            case 'motion': return '◉'
            default: return '•'
        }
    }

    return (
        <div className="toast-container">
            {toasts.map(toast => (
                <div 
                    key={toast.id} 
                    className={`toast ${toast.type}`}
                    onClick={() => removeToast(toast.id)}
                    style={{ cursor: 'pointer' }}
                >
                    <div className="toast-icon">
                        {getIcon(toast.type)}
                    </div>
                    <div className="toast-message">
                        {toast.message}
                    </div>
                </div>
            ))}
        </div>
    )
}

export default ToastContainer