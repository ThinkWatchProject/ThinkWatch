export interface Team {
  id: string;
  name: string;
  description: string | null;
  member_count: number;
  created_at: string;
}

export interface TeamSummary {
  id: string;
  name: string;
}

export interface TeamMember {
  user_id: string;
  email: string;
  display_name: string;
  joined_at: string;
}
