import { inject } from '@angular/core';
import { Router, Routes } from '@angular/router';
import { Settings } from './settings';

const hasServer = () => !!inject(Settings).server() || inject(Router).parseUrl('/login');

export const routes: Routes = [
  { path: 'login', loadComponent: () => import('./login/login').then((m) => m.Login) },
  {
    path: '',
    canActivate: [hasServer],
    children: [
      { path: '', loadComponent: () => import('./sessions/sessions').then((m) => m.Sessions) },
      {
        path: 'new',
        loadComponent: () => import('./sessions/new-session').then((m) => m.NewSession),
      },
      { path: 'stats', loadComponent: () => import('./stats/stats').then((m) => m.Stats) },
      {
        path: 'stats/:month',
        loadComponent: () => import('./stats/stats').then((m) => m.Stats),
      },
      {
        path: 'session/:id',
        loadComponent: () => import('./session/session').then((m) => m.Session),
      },
      {
        path: 'session/:id/review/:callId',
        loadComponent: () => import('./review/review').then((m) => m.Review),
      },
    ],
  },
  { path: '**', redirectTo: '' },
];
