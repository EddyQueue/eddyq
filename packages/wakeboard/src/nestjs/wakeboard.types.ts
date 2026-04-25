export interface EddyqWakeboardOptions {
  /** URL path to mount the wakeboard at. Default: '/wakeboard' */
  mountPath?: string;
  auth?: {
    /** Default: 'admin' */
    username?: string;
    password: string;
  };
}
