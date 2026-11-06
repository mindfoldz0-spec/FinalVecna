import { createBrowserRouter, Navigate } from 'react-router-dom';
import Caller from './routes/Caller';
import Agent from './routes/Agent';

export const router = createBrowserRouter([
  { path: '/', element: <Navigate to="/caller" replace /> },
  { path: '/caller', element: <Caller /> },
  { path: '/agent', element: <Agent /> },
]);
