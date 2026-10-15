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
        path: 'session/:id',
        loadComponent: () => import('./session/session').then((m) => m.Session),
      },
    ],
  },
  { path: '**', redirectTo: '' },
];
