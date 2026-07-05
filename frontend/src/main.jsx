/**
 * Sentinel Command Center
 * Copyright (C) 2026 SourceBox LLC
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published
 * by the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { ClerkProvider } from "@clerk/clerk-react"
import { BrowserRouter } from "react-router-dom"
import './index.css'
import App from './App.jsx'
import { ToastProvider } from './hooks/useToasts.jsx'
import { PlanInfoProvider } from './hooks/usePlanInfo.jsx'
import { SharedTokenProvider } from './hooks/useSharedToken.jsx'

const PUBLISHABLE_KEY = import.meta.env.VITE_CLERK_PUBLISHABLE_KEY

if (!PUBLISHABLE_KEY) {
  throw new Error("Missing VITE_CLERK_PUBLISHABLE_KEY. Please add it to your .env file.")
}

// Match Clerk's UI to our dark brand. Without this, SignIn / SignUp /
// PricingTable / OrganizationSwitcher render with Clerk's default light
// theme on top of our near-black page background — looks broken. Using
// `variables` (not @clerk/themes) keeps us off the extra dependency.
const clerkAppearance = {
  variables: {
    colorBackground: '#12141c',
    colorPrimary: '#22c55e',
    colorText: '#f4f4f5',
    colorTextSecondary: '#a1a1aa',
    colorTextOnPrimaryBackground: '#04170b',
    colorInputBackground: 'rgba(0, 0, 0, 0.35)',
    colorInputText: '#f4f4f5',
    colorNeutral: '#ffffff',
    colorDanger: '#ef4444',
    colorSuccess: '#22c55e',
    colorWarning: '#f59e0b',
    borderRadius: '10px',
    fontFamily: "'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
  },
}

createRoot(document.getElementById('root')).render(
  <StrictMode>
    <ClerkProvider publishableKey={PUBLISHABLE_KEY} appearance={clerkAppearance}>
      <BrowserRouter>
        <ToastProvider>
          <PlanInfoProvider>
            <SharedTokenProvider>
              <App />
            </SharedTokenProvider>
          </PlanInfoProvider>
        </ToastProvider>
      </BrowserRouter>
    </ClerkProvider>
  </StrictMode>,
)